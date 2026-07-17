// Release builds on Windows are GUI apps (no console window behind the game —
// exports ship this binary); debug keeps the console for logs.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]
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

use floptle_core::math::{DVec3, Vec2, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{Entity, Material, Matter, World};
use floptle_script::ScriptHost;
use floptle_render::{
    FlyCamera, Gpu, Grid, Input, MeshId, Outline, Raster, Raymarch, Retro, TexId,
};
use floptle_scene::{
    MaterialDoc, MatterDoc, ProjectConfigDoc, SceneDoc,
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
mod audio;
mod assets_ui;
mod console;
mod curve_edit;
mod dock;
mod gizmo;
mod hierarchy;
mod history;
mod ide;
mod inspector;
mod lua_support;
mod matter_catalog;
mod net;
mod paint_io;
mod paint_mesh;
mod paint_tex;
mod paint_tex_io;
mod paint_ui;
mod play;
mod prefab;
mod shader_graph;
mod shader_preview;
mod shaders;
mod prefs;
mod project;
mod render_frame;
mod scene_ops;
mod space;
mod scene_tab;
mod selection;
mod shading;
mod terrain_edit;
mod terrain_ui;
mod theme;
mod ui_game;
mod ui_widgets;
mod timeline;
mod vertex_paint;
mod vfx;
mod vfx_inspector;
mod vfx_ui;
mod mixer_ui;
mod viewports;
mod viz;

use assets::*;
use console::*;
use dock::*;
use gizmo::*;
use ide::*;
use inspector::*;
use prefs::*;
use shading::*;
use paint_mesh::PaintMeshCache;
use paint_ui::VertexBrush;
use terrain_ui::*;
use vertex_paint::{PaintBlocks, PaintViz};
use theme::*;
use viz::*;

/// Deferred editor commands raised by the UI inside `run_ui`, applied after the
/// frame (so they can call `&mut self` methods the UI closure can't reach).
/// Rect-tool UI resize payload: (entity index, size delta, min-edge per axis,
/// current solved design size).
pub(crate) type UiResize = (u32, [f32; 2], [bool; 2], [f32; 2]);
/// A script's declared defaults: (numeric params, reference params + kinds).
pub(crate) type ScriptDefaults = floptle_script::ScriptDefaults;

/// What Stop restores that the scene doc doesn't carry: the terrain fields
/// (keyed by terrain id) and the terrain texture palette.
pub(crate) type PlayTerrains = (Vec<(u32, floptle_field::ChunkField)>, Vec<String>);

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
    /// Open a folder in the OS file manager (empty path = the project root).
    open_folder: Option<PathBuf>,
    /// Autosave recovery prompt answered: true = restore it, false = discard.
    autosave_action: Option<bool>,
    /// A script file dropped onto a specific hierarchy node (path, entity).
    drop_script_on: Option<(String, Entity)>,
    /// Save a material as a named preset under assets/materials/.
    save_material: Option<(String, MaterialDoc)>,
    /// Give an entity a default Material component (start customizing its look).
    add_material: Option<Entity>,
    /// Add / remove a physics RigidBody on this entity.
    add_rigidbody: Option<Entity>,
    remove_rigidbody: Option<Entity>,
    add_celestial: Option<Entity>,
    remove_celestial: Option<Entity>,
    /// Add a Networked (replication) component on this entity.
    add_networked: Option<Entity>,
    /// Multiplayer harness intents (the 🌐 panel).
    net_host_local: bool,
    net_join_local: bool,
    net_play_as_client: bool,
    net_stop_session: bool,
    /// Host a REAL session on this UDP port (QUIC — the 🌐 panel / net.host{port}).
    net_host_quic: Option<u16>,
    /// Join a real session at this address (host:port).
    net_join_quic: Option<String>,
    /// Host through a rendezvous relay at this address.
    net_host_relay: Option<String>,
    /// Export the project as a runnable game build: (folder, target index —
    /// see `EXPORT_TARGETS`).
    export_game: Option<(String, usize)>,
    /// Add ⏵ UI: create a game-UI node (layer/panel/text/image).
    add_ui: Option<crate::ui_game::AddUi>,
    /// Scene-view UI drag: (element entity index, accumulated design-unit delta).
    ui_move: Option<(u32, [f32; 2])>,
    /// Rect-tool resize of a UI element (Scene tab handles): entity index,
    /// size delta (design units), which edge per axis (true = min/left/top),
    /// and the element's current solved design size (for %-mode scaling).
    ui_resize: Option<UiResize>,
    /// The pointer is over an interactive Scene-view UI overlay (an element rect
    /// or a Rect-tool handle) — those egui interacts own the click, so the raw
    /// viewport press must not gizmo-grab or pick (picking can't see 2D elements
    /// and would clear the selection out from under the drag).
    ui_hot: bool,
    /// Attach an AudioSource component (empty clip — picked in the Inspector).
    add_audio: Option<Entity>,
    remove_audio: Option<Entity>,
    /// Play a clip flat through the editor engine (asset-browser preview).
    preview_audio: Option<String>,
    /// The mixer graph changed (Mixer tab / rename / delete) — live-apply it
    /// to the engine and the running play session.
    mixer_changed: bool,
    /// Attach a ParticleSystem component referencing an existing effect asset.
    add_particles: Option<(Entity, String)>,
    /// Create a starter `.vfx.ron` effect and attach it to this entity.
    new_particles: Option<Entity>,
    remove_particles: Option<Entity>,
    /// Open an effect (by key) in the Particles tab and focus it.
    open_particle_editor: Option<String>,
    /// Bring the Particles tab to the front (re-adding it if closed).
    focus_particles: bool,
    /// Toggle the static MeshCollider marker on a Mesh node (`true` = add, `false` = remove).
    set_mesh_collider: Option<(Entity, bool)>,
    /// Toggle the static Collidable marker on any node (`true` = add, `false` = remove).
    set_collidable: Option<(Entity, bool)>,
    /// Toggle the Trigger flag on a Collidable (sensor: events, no blocking).
    set_trigger: Option<(Entity, bool)>,
    /// A STRUCTURAL physics edit happened (e.g. the Rigidbody mode dropdown) —
    /// rebuild the live sim so bodies/colliders re-register.
    rebuild_physics: bool,
    /// Put a node on a named collision/query layer ("Default" removes the
    /// component). Rebuilds the sim mid-play so static colliders re-layer.
    set_layer: Option<(Entity, String)>,
    /// A project layer was renamed in Project Settings: (old, new). The open
    /// scene's nodes follow the rename (per keystroke, so they stay in sync).
    rename_layer: Option<(String, String)>,
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
    /// Flood the selected node with the brush color (🖌 Paint tab).
    paint_fill: bool,
    /// Strip all paint from the selected node (🖌 Paint tab).
    paint_clear: bool,
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
    /// Open a `.flsl` in the ◈ Shaders graph tab.
    open_shader_graph: Option<String>,
    /// The graph tab's ✚ New: after `new_shader_in` runs, open the fresh file
    /// in the graph (instead of only the text editor).
    new_shader_to_graph: bool,
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
    /// Create a new `.flsl` shader inside this directory (absolute path).
    new_shader_in: Option<String>,
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
    /// Delete these asset files/folders (absolute paths) — opens the confirm.
    delete_asset: Option<Vec<String>>,
    /// Save these nodes (whole subtrees) as ONE prefab file in the folder.
    save_prefab: Option<(Vec<Entity>, PathBuf)>,
    /// Place a prefab instance: (asset path, optional parent node). No parent =
    /// spawn in front of the camera; a parent keeps the authored local offset.
    instantiate_prefab: Option<(String, Option<Entity>)>,
    /// Move these asset files/folders (absolute paths) into a destination folder.
    move_assets: Option<(Vec<String>, PathBuf)>,
    /// Extract a model's embedded animation clips to assets/animations/ (a model path).
    extract_anims: Option<String>,
    /// Attach / change / remove a node's AnimationController: (entity, Some(key) | None).
    set_anim_controller: Option<(Entity, Option<String>)>,
    /// Open the Animation Controller graph window on this controller asset key.
    open_anim_graph: Option<String>,
    /// Open the graph window with the new-controller name prompt; the inner Entity
    /// (if any) gets the created controller attached.
    new_anim_controller: Option<Option<Entity>>,
    /// Focus (or open) the ✏ Animating dock tab.
    focus_animating: bool,
    /// Focus (or open) the ◎ Controller graph dock tab.
    focus_anim_graph: bool,
    /// CONFIRMED asset deletion (from the delete modal) — actually deletes.
    do_delete_asset: Option<Vec<String>>,
    /// Folder the new controller should be created in (absolute; None = default).
    new_anim_controller_dir: Option<String>,
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

/// A File-menu project action, applied after the frame.
#[derive(Clone)]
enum ProjectAction {
    New(String),
    Open(String),
    Close,
}


/// Renders each dockable tab against borrowed slices of the editor's state, and
/// records UI intents on `cmd` to be applied after the frame.
struct EditorTabViewer<'a> {
    world: &'a mut World,
    selection: &'a mut Vec<Entity>,
    /// Game-UI element outlines for the Scene view (index, rect pts, scale).
    ui_overlay: &'a [(u32, [f32; 4], f32)],
    /// The selected node's reference-param kinds ((script kind, param) → kind),
    /// so ref pickers filter to valid targets.
    ref_kinds: &'a HashMap<(String, String), floptle_script::RefKind>,
    /// Canvas bounds (4 corners per layer, Scene-tab points).
    ui_canvas: &'a [[[f32; 2]; 4]],
    /// A selected armature bone `(mesh entity, skeleton node index)` — mutually
    /// exclusive with `selection`; drives the Hierarchy highlight + Inspector bone editor.
    bone_selection: &'a mut Option<(Entity, usize)>,
    /// Double-clicking a tab toggles it into this slot (maximized full-window).
    fullscreen_tab: &'a mut Option<EditorTab>,
    /// Folders collapsed in the Hierarchy (hide their children).
    collapsed: &'a mut std::collections::HashSet<Entity>,
    /// Per rigged-Mesh entity: its skeleton bones (name + parent index), for the
    /// hierarchy's expandable sub-objects + the inspector bone-attach picker.
    bone_names: &'a HashMap<Entity, Vec<(String, Option<usize>)>>,
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
    /// Compiled `.flsl` shaders — the Inspector's Material section reads the
    /// selected shader's uniform/texture schema (and error) from here.
    flsl_cache: &'a shaders::FlslCache,
    /// Parsed Sdf-stage shaders (Field Shapes) — the Material section falls
    /// back to this schema when the picked shader is `stage sdf`.
    sdf_cache: &'a shaders::SdfCache,
    /// The active Sky shader's uniform schema (empty when no sky shader) — the
    /// Inspector's Skybox section renders knob rows from it into `shader_params`.
    sky_uniforms: &'a [floptle_shader::Uniform],
    /// The component clipboard (read-only here; copy/paste route through `cmd`).
    component_clip: &'a Option<ComponentClip>,
    /// Search text for the Inspector's "➕ Add Component" menu.
    add_component_filter: &'a mut String,
    /// The project's layer names ("Default" first) — the Inspector's layer picker.
    layer_names: &'a [String],
    /// The Inspector's "add tag" text field buffer.
    tag_edit: &'a mut String,
    /// See `Editor::hier_scrolled` — scroll-to-selection bookkeeping.
    hier_scrolled: &'a mut Option<Entity>,
    /// Whether the floating Material Editor window is open.
    show_material_editor: &'a mut bool,
    asset_tree: &'a [AssetEntry],
    /// Per-texture sampling settings (read-only here; changes go via `cmd`).
    texture_settings: &'a HashMap<String, TexSetting>,
    /// The selected camera's live POV preview (if a camera is selected).
    cam_preview: Option<egui::TextureId>,
    /// Whether any camera holds play-mode authority (for the Game tab's warning).
    has_active_camera: bool,
    /// Vertex-paint dock-tab state.
    vertex_brush: &'a mut VertexBrush,
    /// Terrain dock-tab state.
    terrain_brush: &'a mut TerrainBrush,
    /// The cubic voxel edge (world units) new terrains are created at — the ONE
    /// density knob (Terrain 2.0: an honest units-per-voxel, not a cell count).
    terrain_voxel: &'a mut f32,
    terrain_textures: &'a mut Vec<String>,
    /// Per-slot glow bitmask (bit i = slot i self-lit) — the Terrain tab's ✨ toggle.
    terrain_glow: &'a mut u32,
    terrain_present: bool,
    /// Terrain stats for the tab: `(volumes, data chunks, resident bytes)`.
    terrain_stats: Option<(usize, usize, usize)>,
    /// Asset browser view mode (false = tree, true = grid) + the grid's folder.
    assets_grid: &'a mut bool,
    assets_grid_dir: &'a mut PathBuf,
    /// The project root — the directory the asset browser is rooted at.
    project_root: &'a Path,
    selected_asset: &'a mut Option<String>,
    asset_selection: &'a mut Vec<String>,
    ide: &'a mut IdeState,
    /// Errors from the last script frame (shown in the Scripting tab).
    script_errors: &'a [String],
    /// Syntax diagnostic for the active IDE file (line, message) — red squiggle.
    ide_diag: Option<&'a (usize, String)>,
    gizmo: Option<&'a GizmoFrame>,
    /// The terrain brush telegraph to draw over the viewport, if sculpting.
    terrain_viz: Option<&'a TerrainViz>,
    /// The vertex-paint brush telegraph, if the Paint tool is hovering a mesh.
    paint_viz: Option<&'a PaintViz>,
    camera_gizmos: &'a [CameraGizmo],
    light_gizmos: &'a [Vec<(Vec2, Vec2)>],
    body_gizmos: &'a [Vec<(Vec2, Vec2)>],
    contact_gizmos: &'a [(Vec2, Vec2)],
    /// Script `gizmo.*` debug lines (projected px + 0-1 color) — Scene view only.
    script_gizmo_lines: &'a [(Vec2, Vec2, [f32; 3])],
    terrain_wire: &'a [(Vec2, Vec2)],
    mesh_wire: &'a [(Vec2, Vec2)],
    /// Selected particle track's emitter/force gizmo (colored screen segments).
    particle_gizmo: &'a [(Vec2, Vec2, [f32; 3])],
    show_gizmos: &'a mut bool,
    gizmo_filter: &'a mut GizmoFilter,
    grabbed: Option<Handle>,
    tool: Tool,
    scene_rect: &'a mut Option<egui::Rect>,
    /// The Game tab's rect (captured each frame it draws), so the editor can size the
    /// Game viewport target to it on the next frame.
    game_rect: &'a mut Option<egui::Rect>,
    /// When true the Game tab paints its own offscreen render (`game_tex`), sized+blit to
    /// the tab rect, instead of showing the full-window surface through a transparent tab.
    /// Fires whenever a docked (non-fullscreen) Game tab is front — single-view or split —
    /// so the game view is always framed to its panel and never spills behind other tabs.
    game_offscreen: bool,
    game_tex: Option<egui::TextureId>,
    aspect: &'a mut AspectMode,
    zoom: &'a mut f32,
    scene_name: &'a str,
    ppp: f32,
    /// The selected code-editor theme index (into `CODE_THEMES`) for the Scripting tab.
    code_theme: usize,
    /// Animation registries + live runtimes (the animation UI reads/edits them).
    anim: &'a mut anim::AnimSystem,
    /// Particle effect registry + preview (the inspector and Particles tab).
    vfx: &'a mut vfx::VfxSystem,
    /// Particles tab UI state.
    vfx_ui: &'a mut vfx_ui::VfxUiState,
    /// The audio system (clip cache, engine, meters — the Mixer tab + previews).
    audio: &'a mut audio::AudioSystem,
    /// Mixer tab UI state.
    mixer_ui: &'a mut mixer_ui::MixerUiState,
    /// The project-wide mixer graph being edited (saved with the project).
    mixer: &'a mut floptle_audio::MixerDesc,
    /// The Particles tab is visible this frame — so the Inspector swaps to the
    /// selected track's settings (VFX artists edit tracks in the Inspector, not a
    /// cramped bottom panel).
    particles_active: bool,
    /// Animation UI state (graph window + Animating tab).
    anim_ui: &'a mut anim_ui::AnimUiState,
    /// The ◈ Shaders tab: the node-graph view of one `.flsl`.
    shader_graph: &'a mut shader_graph::ShaderGraphState,
    /// The graph's per-node preview atlas (tiles drawn on the nodes).
    shader_preview: &'a mut shader_preview::ShaderGraphPreview,
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

    // Form-style panels scroll VERTICALLY ONLY. The dock wraps tab bodies in a
    // two-axis scroll area by default, so one over-wide row (a long script or
    // param name) grows the content region past the visible panel — and every
    // right-aligned control (the … component menus) then aligns to an edge
    // that's off-screen. Vertical-only clamps rows to the panel width, so
    // "right-aligned" always means the VISIBLE right edge and long text
    // truncates instead of pushing controls out of view.
    fn scroll_bars(&self, tab: &EditorTab) -> [bool; 2] {
        match tab {
            EditorTab::Hierarchy | EditorTab::Inspector | EditorTab::Terrain | EditorTab::Paint => {
                [false, true]
            }
            _ => [true, true],
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut EditorTab) {
        match tab {
            EditorTab::Hierarchy => self.hierarchy_ui(ui),
            EditorTab::Inspector => self.inspector_ui(ui),
            EditorTab::Terrain => self.terrain_ui(ui),
            EditorTab::Paint => self.paint_ui(ui),
            EditorTab::Assets => self.assets_ui(ui),
            EditorTab::Console => self.console_ui(ui),
            // Scene = editor free-fly view (tools/gizmos); Game = active-camera view.
            EditorTab::Scene => self.scene_ui(ui, false),
            EditorTab::Game => self.scene_ui(ui, true),
            EditorTab::Scripting => self.scripting_ui(ui),
            EditorTab::Animation => self.animating_ui(ui),
            EditorTab::AnimGraph => self.anim_graph_tab_ui(ui),
            EditorTab::Particles => self.particles_ui(ui),
            EditorTab::Mixer => self.mixer_ui(ui),
            EditorTab::ShaderGraph => self.shader_graph_ui(ui),
        }
    }
}

/// The version THIS distributed build reports — the authority for what a Hub-installed
/// bundle stamps into projects. A packaged bundle carries a `version.json` next to the
/// executable (written by scripts/package.sh / the release CI) whose `version` is the label
/// the Hub installed it under; a bare `cargo run` has no such file and falls back to the
/// compiled-in [`floptle_core::ENGINE_VERSION`] (`0.0.0` in-workspace). Without this, every
/// bundle would report `0.0.0` regardless of its real version — so a "0.1.0" install would
/// pin new projects to an un-installable `0.0.0`.
fn distribution_version() -> String {
    let from_bundle = std::env::current_exe().ok().and_then(|exe| {
        let json = std::fs::read_to_string(exe.with_file_name("version.json")).ok()?;
        json_string_field(&json, "version")
    });
    from_bundle.unwrap_or_else(|| floptle_core::ENGINE_VERSION.to_string())
}

/// Pull `"<key>": "<value>"` out of a flat JSON object without pulling in a JSON parser
/// (the editor has no serde_json dep, and version.json is a tiny machine-written file).
/// Returns `None` if the key or its string value is absent/malformed.
fn json_string_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let after = &json[json.find(&needle)? + needle.len()..];
    let after = after.trim_start().strip_prefix(':')?.trim_start();
    let rest = after.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn main() {
    env_logger::init();
    // CLI surface the Hub (docs/hub-proposal.md) drives. --version / --new / --migrate run
    // HEADLESS (no window or GPU) and exit; a positional path opens that project instead
    // of the default `assets/`.
    let args: Vec<String> = std::env::args().collect();
    // The version to stamp into a scaffolded/migrated project. Defaults to this build's
    // distribution version, but the Hub passes `--engine-version <v>` to pin the EXACT
    // install it chose (the authority is the Hub's `versions/<v>/` dir name, not the
    // binary's compiled-in version) — position-independent, so scan for it first.
    let version_override = args
        .iter()
        .position(|a| a == "--engine-version")
        .and_then(|p| args.get(p + 1))
        .filter(|v| !v.starts_with('-'))
        .cloned();
    let stamp = version_override.unwrap_or_else(distribution_version);
    let mut project_path: Option<PathBuf> = None;
    let mut player_mode = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--version" | "-V" => {
                println!("{} {}", floptle_core::ENGINE_NAME, distribution_version());
                return;
            }
            "--help" | "-h" => {
                println!(
                    "{} editor {}\n\nUSAGE:\n  floptle-editor [PROJECT_DIR]              open a project (default: assets/)\n  floptle-editor --play [PROJECT_DIR]      run the project as a GAME (no editor UI; F1 = multiplayer menu)\n  floptle-editor --new <DIR>               scaffold a new project and exit\n  floptle-editor --migrate <DIR>           migrate a project's assets to this version and exit\n  floptle-editor --engine-version <V>      version to stamp for --new/--migrate (Hub-driven)\n  floptle-editor --version                 print the engine version and exit\n\nA floptle-game.ron manifest next to the binary (File \u{2192} Export Game\u{2026}) implies --play.",
                    floptle_core::ENGINE_NAME, distribution_version()
                );
                return;
            }
            // Consumed by the pre-scan above; skip the flag and its value.
            "--engine-version" => {
                i += 2;
                continue;
            }
            "--new" => {
                let Some(p) = args.get(i + 1).filter(|p| !p.starts_with('-')) else {
                    eprintln!("--new needs a <dir>");
                    std::process::exit(2);
                };
                std::process::exit(new_project(Path::new(p), &stamp));
            }
            "--migrate" => {
                let Some(p) = args.get(i + 1).filter(|p| !p.starts_with('-')) else {
                    eprintln!("--migrate needs a <dir>");
                    std::process::exit(2);
                };
                std::process::exit(migrate_project(Path::new(p), &stamp));
            }
            "--play" => player_mode = true,
            s if !s.starts_with('-') => project_path = Some(PathBuf::from(s)),
            other => {
                eprintln!("unknown argument: {other} (try --help)");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    // An exported build: a `floptle-game.ron` manifest next to the binary makes
    // this process a GAME, not an editor — the project rides alongside it.
    let mut game_title = String::new();
    if !player_mode
        && project_path.is_none()
        && let Some((manifest, dir)) = load_game_manifest()
    {
        player_mode = true;
        game_title = manifest.title;
        project_path = Some(dir.join(manifest.project));
    }

    println!("{} editor v{}", floptle_core::ENGINE_NAME, distribution_version());
    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    // Gizmos/overlays on by default (toggle in the viewport) — but never in a build.
    let mut editor =
        Editor { show_gizmos: !player_mode, player_mode, game_title, ..Default::default() };
    if let Some(p) = project_path {
        editor.project_root = p;
    }
    event_loop.run_app(&mut editor).expect("run editor");
}

/// The manifest File ⏵ Export Game… writes next to the binary. Its presence
/// turns the binary into a game player; `project` is the assets folder
/// relative to the manifest.
#[derive(serde::Serialize, serde::Deserialize)]
struct GameManifest {
    title: String,
    project: String,
}

/// A `floptle-game.ron` beside the running binary, if any → (manifest, its dir).
fn load_game_manifest() -> Option<(GameManifest, PathBuf)> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let text = std::fs::read_to_string(dir.join("floptle-game.ron")).ok()?;
    match ron::from_str::<GameManifest>(&text) {
        Ok(m) => Some((m, dir)),
        Err(e) => {
            eprintln!("floptle-game.ron next to the binary is invalid ({e}); starting as editor");
            None
        }
    }
}

impl Editor {
    /// Export Game… clicked: native targets export immediately from our own
    /// binary; cross targets spawn a background `cargo build` and the export
    /// finishes when it lands (see [`Self::poll_export_build`]).
    fn begin_export(&mut self, dir: String, target: usize) {
        let dir = self.resolve_export_dir(&dir).display().to_string();
        let title = if self.export_title.trim().is_empty() {
            self.project_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "game".into())
        } else {
            self.export_title.trim().to_string()
        };
        let target = target.min(EXPORT_TARGETS.len() - 1);
        let t = &EXPORT_TARGETS[target];
        match t.kind {
            ExportKind::SelfBinary => {
                // A `cargo run` (debug) editor must not ship ITSELF — a debug
                // binary is huge (~600 MB) and slow. With the source checkout
                // around, build the release binary in the background exactly
                // like a cross target. A release editor (a Hub install, where
                // no checkout exists) IS the shipping binary: export directly.
                if cfg!(debug_assertions) && repo_root().is_some() {
                    self.begin_export_build(None, dir, title, target);
                } else {
                    let result =
                        std::env::current_exe().map_err(|e| e.to_string()).and_then(|exe| {
                            export_game_with(&self.project_root, Path::new(&dir), &title, &exe, t)
                        });
                    self.finish_export(result);
                }
            }
            ExportKind::Prebuilt(rel) => {
                let result = repo_root()
                    .ok_or_else(|| {
                        "prebuilt-binary exports need the engine source checkout this editor \
                         was built from"
                            .to_string()
                    })
                    .and_then(|root| {
                        let bin = root.join(rel);
                        if bin.is_file() {
                            export_game_with(&self.project_root, Path::new(&dir), &title, &bin, t)
                        } else {
                            Err(format!(
                                "no {} binary at {} — push the repo, run the `macos-binary` \
                                 workflow (GitHub ⏵ Actions ⏵ Run workflow), download its \
                                 artifact, untar, and put the binary there",
                                t.label,
                                bin.display()
                            ))
                        }
                    });
                self.finish_export(result);
            }
            ExportKind::CargoCross(triple) => {
                self.begin_export_build(Some(triple), dir, title, target);
            }
        }
    }

    /// Spawn the background release build an export waits on (a cross target,
    /// or the host release binary when a debug editor exports "This machine").
    fn begin_export_build(
        &mut self,
        triple: Option<&'static str>,
        out_dir: String,
        title: String,
        target: usize,
    ) {
        if self.export_build.is_some() {
            self.export_status = Some("a target build is already running…".into());
            return;
        }
        let log = std::env::temp_dir().join("floptle-export-build.log");
        match spawn_export_build(triple, &log) {
            Ok(child) => {
                self.export_status = Some(format!(
                    "🔨 building the release {} binary — the export finishes when it's done \
                     (first build takes minutes; log: {})",
                    EXPORT_TARGETS[target].label,
                    log.display()
                ));
                self.export_build =
                    Some(ExportBuild { child, out_dir, title, target, log, started: Instant::now() });
            }
            Err(e) => self.finish_export(Err(e)),
        }
    }

    fn finish_export(&mut self, result: Result<(String, PathBuf), String>) {
        let (level, line) = match result {
            Ok((msg, dir)) => {
                self.export_done = Some(dir);
                (floptle_script::LogLevel::Debug, format!("✅ {msg}"))
            }
            Err(e) => {
                self.export_done = None;
                (floptle_script::LogLevel::Error, format!("📦 export failed: {e}"))
            }
        };
        self.console.push(level, line.clone(), None);
        self.export_status = Some(line);
    }

    /// Where a typed export folder actually lands: absolute paths as-is;
    /// relative paths resolve against the PROJECT's parent folder (predictable
    /// and next to your work — never the process's working directory, which
    /// depends on how the editor was launched).
    pub(crate) fn resolve_export_dir(&self, dir: &str) -> PathBuf {
        let p = Path::new(dir.trim());
        if p.is_absolute() {
            return p.to_path_buf();
        }
        // A relative project root (the default `assets/`) would make the result
        // CWD-relative after all — pin it to the CWD explicitly so the resolved
        // path we display is the path we actually write.
        let root = if self.project_root.is_absolute() {
            self.project_root.clone()
        } else {
            std::env::current_dir().unwrap_or_default().join(&self.project_root)
        };
        root.parent().map(Path::to_path_buf).unwrap_or(root).join(p)
    }

    /// Once per frame: reap a finished cross-target build and complete its
    /// export with the binary it produced.
    pub(crate) fn poll_export_build(&mut self) {
        let done = match self.export_build.as_mut() {
            Some(b) => !matches!(b.child.try_wait(), Ok(None)),
            None => return,
        };
        if !done {
            return;
        }
        let mut b = self.export_build.take().expect("checked above");
        let status = b.child.wait();
        let t = &EXPORT_TARGETS[b.target];
        let triple = match t.kind {
            ExportKind::CargoCross(tr) => Some(tr),
            _ => None,
        };
        let result = match status {
            Ok(s) if s.success() => {
                match export_binary_path(triple, t.exe_suffix) {
                    Some(bin) if bin.is_file() => export_game_with(
                        &self.project_root,
                        Path::new(&b.out_dir),
                        &b.title,
                        &bin,
                        t,
                    )
                    .map(|(m, d)| {
                        (format!("{m} (built in {:.0} s)", b.started.elapsed().as_secs_f32()), d)
                    }),
                    Some(bin) => Err(format!(
                        "the build succeeded but its binary wasn't at {} — rebuild, or report this",
                        bin.display()
                    )),
                    None => Err("the build succeeded but its binary wasn't found".into()),
                }
            }
            Ok(s) => Err(format!(
                "the {} build failed (exit {}) — full log: {}",
                t.label,
                s.code().map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
                b.log.display()
            )),
            Err(e) => Err(format!("build wait: {e}")),
        };
        self.finish_export(result);
    }
}

/// How an Export Game… target obtains its engine binary.
enum ExportKind {
    /// Copy the running binary — always available, always this platform.
    SelfBinary,
    /// `cargo build` for this Rust triple in the background (cross-compiled
    /// locally — Windows-from-Linux via mingw).
    CargoCross(&'static str),
    /// A CI-built binary the user drops at this repo-relative path (macOS —
    /// Apple's SDK can't leave a Mac, so the `macos-binary` GitHub workflow
    /// builds it and the export consumes the downloaded artifact).
    Prebuilt(&'static str),
}

struct ExportTarget {
    label: &'static str,
    kind: ExportKind,
    exe_suffix: &'static str,
    /// A README dropped into the build, `{exe}` replaced with the binary name
    /// (macOS: Gatekeeper won't run unsigned internet downloads untouched).
    readme: Option<&'static str>,
}

const MAC_README: &str = "\
This is an unsigned build (no Apple notarization yet), so macOS quarantines it
after a download. In Terminal, from this folder, run once:

    xattr -dr com.apple.quarantine .
    chmod +x ./{exe}

Then start the game with:

    ./{exe}

F1 in-game opens the multiplayer menu (host / join with a lobby code).
";

const EXPORT_TARGETS: &[ExportTarget] = &[
    ExportTarget {
        label: "This machine",
        kind: ExportKind::SelfBinary,
        exe_suffix: std::env::consts::EXE_SUFFIX,
        readme: None,
    },
    ExportTarget {
        label: "Windows (x86_64)",
        kind: ExportKind::CargoCross("x86_64-pc-windows-gnu"),
        exe_suffix: ".exe",
        readme: None,
    },
    ExportTarget {
        label: "macOS",
        kind: ExportKind::Prebuilt("prebuilt/floptle-macos"),
        exe_suffix: "",
        readme: Some(MAC_README),
    },
];

/// A cross-target build in flight: the spawned `cargo build`, everything
/// needed to finish the export when it lands, and where its output went.
struct ExportBuild {
    child: std::process::Child,
    out_dir: String,
    title: String,
    target: usize,
    log: PathBuf,
    started: Instant,
}

/// Where an export's background-built binary lands: `<target-dir>/<triple>/release/`
/// for a cross target, `<target-dir>/release/` for the host (`triple: None`).
/// The running binary is `<target-dir>/<profile>/floptle`, so the target dir is
/// two levels up — true for `cargo run` and plain `cargo build` alike.
fn export_binary_path(triple: Option<&str>, exe_suffix: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let target_dir = exe.parent()?.parent()?;
    let name = exe.file_stem()?.to_string_lossy().into_owned();
    let release = match triple {
        Some(tr) => target_dir.join(tr).join("release"),
        None => target_dir.join("release"),
    };
    Some(release.join(format!("{name}{exe_suffix}")))
}

/// The mingw cross toolchain for Windows exports: system-wide (PATH) or the
/// user-space llvm-mingw install (`~/.local/opt/llvm-mingw`) — portable, no
/// root needed. Returns the bin dir to prepend to the child's PATH (None =
/// already on PATH).
fn windows_toolchain_bin() -> Result<Option<PathBuf>, String> {
    let cc = "x86_64-w64-mingw32-gcc";
    let on_path = std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p).any(|d| d.join(cc).is_file() || d.join(format!("{cc}.exe")).is_file())
    });
    if on_path {
        return Ok(None);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let bin = PathBuf::from(home).join(".local/opt/llvm-mingw/bin");
        if bin.join(cc).is_file() {
            return Ok(Some(bin));
        }
    }
    Err(format!(
        "no Windows cross-toolchain: install llvm-mingw to ~/.local/opt/llvm-mingw \
         (portable, no root) or `{cc}` system-wide (e.g. pacman -S mingw-w64-gcc)"
    ))
}

/// The engine source checkout this editor was built from (compiled-in path —
/// a dev machine, which is where exports happen today). `None` if it's gone.
fn repo_root() -> Option<PathBuf> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?.parent()?;
    repo.join("Cargo.toml").is_file().then(|| repo.to_path_buf())
}

/// Spawn the background release `cargo build` for an export: the host binary
/// (`triple: None`) or a cross target. Needs the engine source checkout and,
/// for cross targets, the rustup + C toolchains.
fn spawn_export_build(triple: Option<&str>, log: &Path) -> Result<std::process::Child, String> {
    let repo = repo_root().ok_or_else(|| {
        "built exports need the engine source checkout this editor was built from".to_string()
    })?;
    let logfile = std::fs::File::create(log).map_err(|e| format!("build log: {e}"))?;
    let mut cmd = std::process::Command::new("cargo");
    cmd.current_dir(repo)
        .args(["build", "--release", "-p", "floptle-editor"])
        .stdout(logfile.try_clone().map_err(|e| e.to_string())?)
        .stderr(logfile)
        .stdin(std::process::Stdio::null());
    if let Some(tr) = triple {
        cmd.args(["--target", tr]);
    }
    // Build into the SAME target dir `cross_binary_path` reads (the running
    // editor's). Without this the child cargo used whatever CARGO_TARGET_DIR
    // the environment happened to have — launched differently, the build
    // succeeded in one place while the export looked in another and reported
    // failure over a perfectly good build.
    if let Some(td) = std::env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.parent()?.to_path_buf()))
    {
        cmd.env("CARGO_TARGET_DIR", td);
    }
    if triple == Some("x86_64-pc-windows-gnu") {
        if let Some(bin) = windows_toolchain_bin()? {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut paths = vec![bin.clone()];
            paths.extend(std::env::split_paths(&path));
            cmd.env("PATH", std::env::join_paths(paths).map_err(|e| e.to_string())?);
            // llvm-mingw ships compiler-rt/libunwind, but rustc's windows-gnu
            // target links `-lgcc`/`-lgcc_eh` — alias them to libunwind once
            // and point the build at the shim. (A real mingw-w64-gcc on PATH
            // has libgcc and skips all of this.)
            let root = bin.parent().ok_or("llvm-mingw layout")?;
            let shim = root.join("rust-shim");
            let unwind = root.join("x86_64-w64-mingw32/lib/libunwind.a");
            if !shim.join("libgcc.a").is_file() {
                std::fs::create_dir_all(&shim).map_err(|e| format!("shim dir: {e}"))?;
                std::fs::copy(&unwind, shim.join("libgcc.a"))
                    .and_then(|_| std::fs::copy(&unwind, shim.join("libgcc_eh.a")))
                    .map_err(|e| format!("libgcc shim: {e}"))?;
            }
            let mut rustflags =
                std::env::var("RUSTFLAGS").unwrap_or_default();
            rustflags.push_str(&format!(" -L {}", shim.display()));
            cmd.env("RUSTFLAGS", rustflags.trim());
        }
        cmd.env("CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER", "x86_64-w64-mingw32-gcc");
    }
    cmd.spawn().map_err(|e| format!("spawn cargo: {e}"))
}

/// File ⏵ Export Game…: stamp out a runnable build — an engine binary + the
/// project's assets + the `floptle-game.ron` manifest that flips it into
/// player mode. `binary` is the engine to ship: our own executable for a
/// native export, a cross-compiled one a background build produced, or a
/// CI-built prebuilt (macOS).
fn export_game_with(
    project_root: &Path,
    out: &Path,
    title: &str,
    binary: &Path,
    target: &ExportTarget,
) -> Result<(String, PathBuf), String> {
    let exe_suffix = target.exe_suffix;
    std::fs::create_dir_all(out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let proj = project_root.canonicalize().map_err(|e| format!("project dir: {e}"))?;
    let out_c = out.canonicalize().map_err(|e| format!("export dir: {e}"))?;
    if out_c.starts_with(&proj) {
        return Err("the export folder can't be inside the project (it would copy itself)".into());
    }
    // A build that can't find its chosen entry scene is dead on arrival — catch
    // it at export time, not on a player's machine.
    let cfg = floptle_scene::load_project(&proj.join("project.ron"));
    if let Some(entry) = cfg.entry_scene.as_deref()
        && !proj.join(entry).is_file()
    {
        return Err(format!(
            "the project's entry scene ({entry}) doesn't exist — pick one in \
             Edit ⏵ Project Settings"
        ));
    }
    // Binary name from the title: filesystem-safe, the TARGET's suffix.
    let stem: String = title
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let stem = stem.trim_matches('_');
    let stem = if stem.is_empty() { "game" } else { stem };
    let mut exe_name = format!("{stem}{exe_suffix}");
    // The shipped project folder is literally named `assets` — an exe resolving
    // to that same name (a project rooted at `assets/`, exported for a
    // suffix-less target) would collide with it and corrupt the build.
    if exe_name == "assets" {
        exe_name = "game".into();
    }
    // The build's `assets/` copy is wholly owned by the export: clear the
    // previous one so files deleted from the project don't linger in shipped
    // builds (and a stale FILE named `assets` — the old broken-export
    // artifact — doesn't block the copy).
    let ship_assets = out_c.join("assets");
    if ship_assets.is_dir() {
        std::fs::remove_dir_all(&ship_assets).map_err(|e| format!("clear old assets copy: {e}"))?;
    } else if ship_assets.exists() {
        std::fs::remove_file(&ship_assets).map_err(|e| format!("clear old assets copy: {e}"))?;
    }
    // Everything the game needs ships BEFORE the binary, and the binary ships
    // LAST — a failed export must never leave a runnable-looking exe that,
    // missing its floptle-game.ron, silently boots as the EDITOR.
    let files = copy_tree(&proj, &ship_assets).map_err(|e| format!("copy assets: {e}"))?;
    if let Some(tpl) = target.readme {
        std::fs::write(out_c.join("README.txt"), tpl.replace("{exe}", &exe_name))
            .map_err(|e| format!("write README: {e}"))?;
    }
    let manifest = GameManifest { title: title.to_string(), project: "assets".into() };
    let text = ron::ser::to_string_pretty(&manifest, ron::ser::PrettyConfig::default())
        .map_err(|e| format!("manifest: {e}"))?;
    std::fs::write(out_c.join("floptle-game.ron"), text)
        .map_err(|e| format!("write manifest: {e}"))?;
    let shipped = out_c.join(&exe_name);
    std::fs::copy(binary, &shipped).map_err(|e| format!("copy binary: {e}"))?;
    // A CI artifact may have lost its executable bit in transit — restore it
    // (only meaningful for unix-family targets; .exe doesn't care).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&shipped, std::fs::Permissions::from_mode(0o755));
    }
    Ok((format!("exported {exe_name} + {files} asset file(s) to {}", out_c.display()), out_c))
}

/// Recursive copy for the export: skips dot-entries (`.floptle` caches,
/// `.luarc.json` — IDE/editor plumbing a build doesn't need). Returns the
/// number of files copied.
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<u64> {
    std::fs::create_dir_all(dst)?;
    let mut n = 0;
    for entry in std::fs::read_dir(src)?.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            n += copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
            n += 1;
        }
    }
    Ok(n)
}

/// Headless `--new <dir>`: scaffold a project (dirs + default materials/scripts, a starter
/// scene, a `project.ron` pinned to `stamp`) without a window/GPU. `stamp` is the engine
/// version to record — the Hub's chosen install label, or this build's distribution version.
/// Returns the process exit code.
fn new_project(path: &Path, stamp: &str) -> i32 {
    // Refuse to scaffold over an existing project — that would clobber its project.ron.
    if path.join("project.ron").exists() {
        eprintln!("{} already contains a project (project.ron); refusing to overwrite", path.display());
        return 1;
    }
    if let Err(e) = std::fs::create_dir_all(path) {
        eprintln!("could not create {}: {e}", path.display());
        return 1;
    }
    // seed_project_dirs / project_cfg_path only touch the filesystem via project_root, so a
    // Default editor (no GPU) is a valid headless context for them.
    let ed = Editor { project_root: path.to_path_buf(), ..Default::default() };
    ed.seed_project_dirs();
    let scene = path.join("scenes/first.ron");
    if !scene.exists()
        && let Err(e) = floptle_scene::save(&crate::project::default_scene(), &scene)
    {
        eprintln!("could not write starter scene: {e}");
        return 1;
    }
    let cfg = floptle_scene::ProjectConfigDoc {
        engine_version: Some(stamp.to_string()),
        ..floptle_scene::ProjectConfigDoc::default()
    };
    if let Err(e) = floptle_scene::save_project(&cfg, &ed.project_cfg_path()) {
        eprintln!("could not write project.ron: {e}");
        return 1;
    }
    println!("created project at {}", path.display());
    0
}

/// Headless `--migrate <dir>`: re-serialize every `.vfx.ron` (so the clip-emit migration
/// persists) and stamp `project.ron`'s engine_version to `stamp` (the Hub's target install
/// label, or this build's distribution version). Best-effort — a file that fails to parse is
/// left as-is. Returns the process exit code.
fn migrate_project(path: &Path, stamp: &str) -> i32 {
    if !path.is_dir() {
        eprintln!("{} is not a directory", path.display());
        return 1;
    }
    // Recursively re-serialize effects (load runs migrate_clips), skipping hidden/target.
    let mut stack = vec![path.to_path_buf()];
    let mut migrated = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with('.') && name != "target" {
                    stack.push(p);
                }
            } else if p.to_string_lossy().ends_with(floptle_scene::VFX_EXT)
                && let Ok(doc) = floptle_scene::load_vfx_effect(&p)
                && floptle_scene::save_vfx_effect(&doc, &p).is_ok()
            {
                migrated += 1;
            }
        }
    }
    // Stamp the project's engine version — but only if project.ron exists AND parses. Never
    // fabricate a missing one or overwrite an unparseable one (that would lose data).
    let cfg_path = path.join("project.ron");
    match floptle_scene::try_load_project(&cfg_path) {
        Ok(Some(mut cfg)) => {
            cfg.engine_version = Some(stamp.to_string());
            let _ = floptle_scene::save_project(&cfg, &cfg_path);
        }
        Ok(None) => {} // no project.ron — leave it that way.
        Err(e) => eprintln!("leaving project.ron untouched (won't parse: {e})"),
    }
    println!("migrated {migrated} effect(s) in {}", path.display());
    0
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
    /// Billboard particle pass (the VFX sim's draw arm).
    particles: Option<floptle_render::Particles>,
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
    /// The game-UI render pass (instanced quads + glyph atlas).
    ui_render: Option<floptle_render::Ui>,
    /// This frame's Scene-view UI overlay (projected element rects).
    ui_overlay: Vec<(u32, [f32; 4], f32)>,
    /// Canvas bounds gizmos: 4 projected corners per layer (Scene-tab points).
    ui_canvas: Vec<[[f32; 2]; 4]>,
    /// Game-UI interaction state: the element the pointer hovers / grabbed.
    ui_hover: Option<u32>,
    ui_active: Option<u32>,
    /// Last frame's LMB, for press/release edges in the UI interact pass.
    ui_lmb_was: bool,
    /// UI hook events detected this frame, dispatched after the script run.
    ui_events: Vec<(u32, &'static str)>,
    /// Last frame's `cmd.ui_hot`: the cursor sat on a Scene-view UI overlay
    /// interact (element rect / Rect handle), so LMB belongs to egui.
    ui_overlay_hot: bool,
    /// The SELECTED node's reference-param kinds, (script kind, param) → kind —
    /// refreshed by `sync_selected_script_params`, read by the Inspector to
    /// filter ref pickers (script/component refs only list valid targets).
    ref_kinds: HashMap<(String, String), floptle_script::RefKind>,
    /// The sampling each registered texture was last built with, so a settings change
    /// forces a re-register (with the new sampler / mips).
    texture_registry_setting: HashMap<String, TexSetting>,
    /// Per-texture sampling settings (filter + wrap), keyed by image path. Persisted to
    /// `.floptle/textures.ron`. Absent ⏵ the crisp tiling default.
    texture_settings: HashMap<String, TexSetting>,
    /// Editable terrains, keyed by their scene node Entity (each field in its node's
    /// LOCAL space). Empty until "New Terrain". Terrain 2.0: the AUTHORITY is the
    /// sparse unbounded [`floptle_field::ChunkField`] (brushes, physics, save, Lua);
    /// each carries a capped-resolution dense shadow proxy feeding the SDF atlas.
    terrains: HashMap<Entity, crate::terrain_edit::EditorTerrain>,
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
    /// Terrain 2.0 (ADR terrain-mesh): each terrain's PRIMARY-ray rendering is a set of
    /// extracted chunk meshes drawn through the raster pass, instead of sphere-tracing a
    /// voxel field. Meshes extract straight from the authoritative `ChunkField`; the
    /// atlas keeps sun shadows + SDF AO through each terrain's shadow proxy (`w = 3` =
    /// in-field-but-not-drawn). This map is the per-terrain GPU slot set.
    terrain_render: HashMap<Entity, crate::terrain_edit::TerrainRender>,
    /// Chunks whose voxels changed since the last remesh, per terrain — the regional
    /// remesh queue a brush dab (or undo swap) feeds. Drained every frame by
    /// `sync_terrain_meshes`.
    terrain_chunks_dirty: HashMap<Entity, Vec<[i32; 3]>>,
    /// The background remesh worker (P4) — spawned lazily on first terrain use.
    terrain_worker: Option<crate::terrain_edit::TerrainWorker>,
    /// Monotonic job stamp for worker remeshes: never repeats across scenes, so a
    /// stale result from a previous world can never land on a reused entity id.
    terrain_epoch: u64,
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
    /// Pre-stroke chunk snapshots, captured lazily as the stroke's dabs touch new
    /// chunks — pushed to the undo timeline on mouse-up if the stroke actually
    /// deformed the terrain. `None` between strokes. The whole stroke collapses to a
    /// single undo step of only the touched chunks (~MBs, not the whole field).
    stroke_snapshot: Option<(u32, floptle_field::ChunkUndo)>,
    /// At least one dab landed during the current stroke (so it's worth undoing).
    stroke_dabbed: bool,
    /// LMB held with the Paint tool — keep dabbing on mouse motion.
    painting: bool,
    /// Pre-stroke colors captured on the first dab, banked to the undo timeline on
    /// mouse-up. `(paint id, colors per part)`; the whole stroke = one undo step.
    paint_stroke_snapshot: Option<(u32, Vec<Vec<[u8; 4]>>)>,
    /// At least one dab landed during the current paint stroke.
    paint_stroke_dabbed: bool,
    /// TEXTURE-paint stroke undo: id → pre-stroke images, captured the first time the
    /// stroke touches each node (the sphere brush can cross several). `None` = that node
    /// had no paint before, so undo removes it. Banked as ONE history step on mouse-up.
    tex_stroke_snapshot: std::collections::HashMap<u32, Option<Vec<Vec<u8>>>>,
    /// Vertex-paint brush settings.
    vertex_brush: VertexBrush,
    /// Retained CPU geometry + triangle grids for painted meshes (built lazily).
    paint_meshes: PaintMeshCache,
    /// paint id → its per-part blocks in the renderer's `vpaint` store.
    paint_data: std::collections::HashMap<u32, PaintBlocks>,
    /// TEXTURE painting (the ▦ Texture brush target): per-node paint images + atlas meshes,
    /// keyed by the stable `TexturePaint` id so undo survives a World rebuild (see `paint_tex`).
    paint_tex: std::collections::HashMap<u32, crate::paint_tex::PaintTex>,
    /// Bumped on EVERY vertex-paint mutation (dab, fill, clear, undo, reload). Texture-painted
    /// nodes mirror their vertex paint into atlas-ordered blocks; `sync_tex_paint_mirrors`
    /// compares this against each mirror's epoch to rebuild only when something changed.
    vpaint_epoch: u64,
    /// The paint brush telegraph for this frame (projected ring).
    paint_viz: Option<PaintViz>,
    /// Terrain brush settings.
    terrain_brush: TerrainBrush,
    /// New-terrain resolution along the long axis (user-controllable detail).
    /// Cubic voxel edge for NEW terrains, world units (the Terrain tab's density knob).
    terrain_voxel: f32,
    /// Terrain texture palette — image paths per slot (empty = unused).
    terrain_textures: Vec<String>,
    /// Bit i = palette slot i GLOWS (self-lit albedo, bypasses lighting + AO — how
    /// magma veins and cave crystals stay visible underground). Persisted in the
    /// `.palette` sidecar as a `|glow` suffix on the slot's line.
    terrain_glow_mask: u32,
    /// The terrain palette needs re-uploading to the GPU.
    terrain_textures_dirty: bool,
    /// The skybox texture path currently uploaded to the GPU (`None` = solid/white), so
    /// we only re-upload when the skybox node's texture actually changes.
    sky_texture_loaded: Option<String>,
    /// The active Sky shader: `(project-relative path, file mtime, uniform SCHEMA)`.
    /// Recompiled + re-spliced only when the path or mtime changes. `None` = built-in sky.
    /// The schema (name/type/range/default per uniform) both drives the Skybox
    /// Inspector's knob rows and, resolved against the node's `shader_params` each
    /// frame (`sky_uniform_values`), fills `RaymarchGlobals.sky_uniforms` — so a knob
    /// drag takes effect immediately, no recompile.
    sky_shader: Option<(String, u64, Vec<floptle_shader::Uniform>)>,
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
    /// right, or the H key). Off = a clean view; the selected node's collider still
    /// hides too.
    show_gizmos: bool,
    /// Per-category gizmo visibility (the ⏷ menu beside the master toggle) —
    /// tune what draws without giving up the rest.
    gizmo_filter: GizmoFilter,
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
    /// This frame's projected particle-emitter gizmo: the selected track's birth shape,
    /// emit-direction and force arrows, as colored `(a, b, rgb)` screen segments.
    particle_gizmo: Vec<(Vec2, Vec2, [f32; 3])>,
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
    /// The full multi-selection in the browser (Ctrl/Shift-click); the primary is
    /// `selected_asset`. Used for bulk move/delete.
    asset_selection: Vec<String>,
    /// Resolution-simulator framing for the Scene tab.
    aspect_mode: AspectMode,
    viewport_zoom: f32,
    /// The Scene tab's rect (logical points), captured each frame — gates picking.
    scene_rect: Option<egui::Rect>,
    scene_name: String,
    /// Selected entities (multi-select); the gizmo/inspector act on the last one.
    selection: Vec<Entity>,
    /// A selected armature bone `(rigged-mesh entity, skeleton node index)` — clicked in
    /// the Hierarchy's bone tree. Bones aren't ECS entities, so this rides alongside
    /// `selection` (they're mutually cleared) and drives the Inspector's bone editor.
    bone_selection: Option<(Entity, usize)>,
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
    /// Per-GAMEPLAY-TICK input accumulators (docs/netcode-design.md §3): edges and
    /// deltas bank here in parallel with the per-frame sets above, and are consumed
    /// by each `fixedUpdate` tick — so a key pressed between ticks is never lost,
    /// and the per-tick snapshot is exactly what netcode input commands will carry.
    tick_keys_pressed: std::collections::HashSet<String>,
    tick_keys_released: std::collections::HashSet<String>,
    tick_buttons_pressed: [bool; 3],
    tick_mouse_delta: (f32, f32),
    tick_scroll: f32,
    /// The 60 Hz gameplay-tick accumulator driving `fixedUpdate` + physics, and the
    /// tick counter (the netcode timebase). Reset on Play.
    game_tick: floptle_core::FixedTimestep,
    game_tick_no: u64,
    /// The in-editor multiplayer session (docs/netcode-design.md §12 2b): the play
    /// world hosts, an optional ghost-client world joins over the in-process hub
    /// with simulated latency/loss, and cyan gizmos show its view. Torn down on Stop.
    net_hub: Option<floptle_net::MemoryHub>,
    net_server: Option<floptle_net::NetSession>,
    net_client: Option<(floptle_net::NetSession, floptle_core::World)>,
    /// The scene doc captured at host time — the baseline any ghost client loads
    /// (exactly what a remote client would load from disk).
    net_scene_doc: Option<floptle_scene::SceneDoc>,
    /// Harness link conditions: one-way latency in ticks + unreliable-drop chance.
    net_latency_ticks: u64,
    net_loss: f32,
    /// Draw the ghost client's replicated positions as cyan gizmo spheres.
    net_ghosts: bool,
    show_net_panel: bool,
    /// "Test as remote player" (2c): the play world's CLIENT session (the play
    /// world predicts) + the hidden authoritative server behind the link.
    net_play_client: Option<floptle_net::NetSession>,
    net_hidden: Option<net::HiddenServer>,
    /// The play world's predicted node + its rewind-replay bookkeeping.
    net_predictor: Option<(Entity, floptle_net::Predictor)>,
    /// Once-per-play warning that the local test harness drops `terrain.*` edits.
    net_terrain_warned: bool,
    /// Once-per-play warning that a terrain edit couldn't reach the sim's
    /// collider copy (no matching terrain collider) — a silent miss here reads
    /// as "standing on an invisible old surface" and is unfindable later.
    terrain_mirror_warned: bool,
    /// Space time (solar demo S2): seconds of ON-RAILS celestial time, advanced
    /// each gameplay tick by `space_warp × tick_dt`. Drives every
    /// `CelestialBody` node's Kepler position.
    space_time: f64,
    /// Current time-warp multiplier (`space.warp(m)` requests land here).
    space_warp: f64,
    /// Real hosting (QUIC): Predicted nodes owned by REMOTE peers — each runs
    /// its scripts with its owner's replayed input in the tick loop (the
    /// one-script model, server side). Empty on the loopback harness.
    net_remote_predicted: Vec<(Entity, u64)>,
    /// Real hosting: the lag-comp history ring (the hidden harness server
    /// keeps its own inside `HiddenServer`).
    net_history: floptle_net::LagHistory,
    /// 🌐 panel text buffers: the LAN host port, the join address, the relay.
    net_host_port: String,
    net_join_addr: String,
    net_relay_addr: String,
    /// The join-by-code buffer (a five-letter lobby code).
    net_join_code: String,
    /// The live lobby code while hosting via a relay.
    net_lobby_code: Option<String>,
    /// PLAYER MODE (`--play`, or a `floptle-game.ron` manifest next to the
    /// binary — what File ⏵ Export Game… produces): boot straight into Play,
    /// Game view fullscreen, no editor chrome. F1 = the multiplayer menu.
    player_mode: bool,
    /// The window title in player mode (the export manifest's `title`).
    game_title: String,
    /// File ⏵ Export Game… dialog state: visibility, target folder, the game
    /// title to stamp, the build-target index (`EXPORT_TARGETS`), and the
    /// last result line.
    show_export: bool,
    export_dir: String,
    export_title: String,
    export_target: usize,
    export_status: Option<String>,
    /// The last SUCCESSFUL export's folder — powers the dialog's "Open folder".
    export_done: Option<PathBuf>,
    /// When the last crash-recovery autosave was written (see `autosave_tick`).
    last_autosave: Option<Instant>,
    /// An autosave NEWER than the scene file was found at load — the recovery
    /// prompt is up ("restore unsaved work?"); holds the autosave path.
    autosave_prompt: Option<PathBuf>,
    /// A cross-target `cargo build` running in the background (Windows export
    /// from Linux etc.); polled each frame, the export finishes when it does.
    export_build: Option<ExportBuild>,
    /// The tick input snapshot most recently fed to `fixedUpdate` — cloned so
    /// prediction can record + ship exactly what the scripts saw.
    last_tick_input: floptle_script::InputSnapshot,
    /// A script asked (via `input.lockMouse()`) to hold the cursor grabbed + hidden for
    /// free-look. While set, the RMB-release handler won't release the grab, and Stop
    /// releases it. Reset when play ends.
    script_mouse_lock: bool,
    /// The active cursor grab is only a CONFINE (X11 has no OS-level lock): the
    /// cursor can still wander inside the window, so we re-center it every frame.
    cursor_lock_soft: bool,
    /// The Game viewport has trapped the cursor (clicked into it while playing):
    /// the OS cursor is grabbed+hidden and confined to the Game rect, all input goes
    /// to the game, and only Escape (or Stop) releases it. Prevents the mouse from
    /// wandering onto editor panels while you play.
    game_trap: bool,
    /// A middle-mouse pan drag is in progress over the Scene viewport (cursor grabbed
    /// so the raw delta never hits a window edge); `pan_press` restores the pointer
    /// to where the drag began on release.
    panning: bool,
    pan_press: Option<Vec2>,
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
    /// The docked/split Game viewport's own retro pass, sized to the panel's aspect (the
    /// shared `retro` is sized to the window). Lets a docked Game tab pixelate + dither
    /// exactly like the fullscreen view instead of rendering crisp.
    game_retro: Option<Retro>,
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
    /// The OS clipboard (lazy) — node copies also land here as tagged RON, so
    /// paste works across scene switches, editor instances, and projects.
    os_clipboard: Option<egui_winit::clipboard::Clipboard>,
    /// An inspector/gizmo edit session is open — coalesces a drag into one undo step.
    editing: bool,
    /// The pre-edit scene snapshot captured at the start of this frame.
    frame_snapshot: Option<floptle_scene::SceneDoc>,
    /// The dock tab the user is focused in (updated each frame from the dock's
    /// focused leaf). Global scene shortcuts (Delete, arrows, F, Ctrl+C/V/D…) are
    /// suppressed while a timeline tab holds focus so it owns those keys for its
    /// own keyframes — see the key handler and `focused_in_timeline`.
    focused_tab: Option<EditorTab>,
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
    /// Text being typed into the Inspector's "add tag" field.
    tag_edit: String,
    /// The last selection primary the Hierarchy auto-scrolled to (so a viewport
    /// pick scrolls the tree exactly once, not every frame).
    hier_scrolled: Option<Entity>,
    /// Text being typed into Project Settings' "new layer" field.
    layer_new: String,
    /// Play mode: scripts run; the pre-play authored scene is restored on stop.
    playing: bool,
    /// The physics sim while playing (built on Play, dropped on Stop).
    sim: Option<floptle_physics::Sim>,
    /// Paused (in play mode): the script clock freezes.
    paused: bool,
    /// Accumulated play-mode seconds (advances only while playing and not paused).
    play_t: f32,
    play_snapshot: Option<SceneDoc>,
    /// The open scene file as a project-root-relative path ("scenes/first.ron")
    /// — what multiplayer sessions name scenes by on the wire. Kept in lockstep
    /// with `scene_name` by [`Self::set_scene_file`].
    scene_rel: String,
    /// (name, rel) when Play started. A mid-play `scene.load(...)` renames the
    /// scene for the session; Stop restores both alongside the snapshot so the
    /// editor's scene saves back to its own file, not the played one's.
    play_scene_name: Option<(String, String)>,
    /// Parsed prefab files by path (mtime-validated) — `spawn("…")` every tick
    /// must not re-read + re-parse the asset.
    prefab_cache: HashMap<std::path::PathBuf, (std::time::SystemTime, Vec<floptle_scene::NodeDoc>)>,
    /// Compiled `.flsl` shaders by project-relative path (mtime hot reload).
    flsl_cache: shaders::FlslCache,
    /// Live group(3) material bindings per shader-material entity.
    flsl_binds: shaders::FlslBinds,
    /// Retired binding slots, reused before growing the raster's registry.
    flsl_free: Vec<floptle_render::FlslBindingId>,
    /// Parsed Sdf-stage shaders by material path (Field Shapes, mtime-cached).
    sdf_cache: shaders::SdfCache,
    /// Live Field Shape entities → their splice slot (0..4).
    flsl_shape_slots: HashMap<Entity, usize>,
    /// The (entity, shader, generation) set the current splice was built from.
    flsl_field_key: Vec<(Entity, String, u64)>,
    /// The ◈ Shaders tab: the node-graph view of one `.flsl`.
    shader_graph: shader_graph::ShaderGraphState,
    /// The graph's live per-node preview atlas (pipeline + egui texture).
    shader_preview: shader_preview::ShaderGraphPreview,
    /// The terrain fields (id-keyed) + texture palette when Play started.
    /// Terrain lives OUTSIDE the scene doc, so `play_snapshot` doesn't carry
    /// it — Stop restores from here so unsaved sculpts survive Play and a
    /// mid-play scene switch can't leak another scene's terrain into this one.
    play_terrains: Option<PlayTerrains>,
    /// A `scene.load(...)` a script queued this frame — performed at the top of
    /// the NEXT frame (never mid-frame under the running scripts).
    pending_scene: Option<String>,
    /// The display's refresh period, seconds (0 = unknown) — dt snaps to whole
    /// multiples of it so scheduler noise never reaches the simulation clock.
    refresh_period: f32,
    /// Frames until the refresh rate is re-queried (the window can change monitors).
    refresh_poll: u32,
    /// Banked (raw − snapped) dt, folded back ≤0.25 ms/frame — keeps long-term
    /// time wall-clock exact under dt snapping.
    dt_snap_error: f32,
    /// The Lua VM that runs node scripts in play mode (ADR-0003).
    script_host: ScriptHost,
    /// Animation: clip/controller registries + live per-entity runtimes.
    anim: anim::AnimSystem,
    /// Particles: effect registry + live play-mode instances.
    vfx: vfx::VfxSystem,
    /// Audio: the sound engine, clip cache, play-mode voices, mixer state.
    audio: audio::AudioSystem,
    /// Mixer tab UI state (selected track/effect, meters).
    mixer_ui: mixer_ui::MixerUiState,
    /// Particles tab UI state (open effect, playhead, selections).
    vfx_ui: vfx_ui::VfxUiState,
    /// Animation UI state (graph window + Animating tab).
    anim_ui: anim_ui::AnimUiState,
    /// Errors from the most recent script frame, shown in the Scripting tab.
    script_errors: Vec<String>,
    /// Cache of each script file's declared `defaults` keyed by path, with the file's
    /// mtime so we only re-parse when it changes — drives live inspector param sync.
    script_defaults_cache: HashMap<String, (std::time::SystemTime, ScriptDefaults)>,
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
    /// A close was decided (Save & Quit / Quit without saving): the winit loop exits on
    /// the next `about_to_wait`. A plain flag — NOT `ViewportCommand::Close`, which is an
    /// eframe/multi-viewport command this raw winit + egui-winit app never acts on (that
    /// was the "click Save & Exit, nothing closes" bug).
    pending_exit: bool,
    /// A short-lived on-screen confirmation (message, seconds remaining) — so a save is
    /// visibly acknowledged instead of only whispering to the Console.
    toast: Option<(String, f32)>,
    /// An asset delete awaiting confirmation (absolute path).
    delete_confirm: Option<Vec<String>>,
    last: Option<Instant>,
    started: Option<Instant>,
    gpu: Option<Gpu>,
}

/// One reversible step on the unified timeline. Scene edits store a whole-scene
/// doc; terrain strokes store the field's serialized bytes. Keeping both kinds on
/// one stack means Ctrl+Z walks back through scene + terrain edits in true order.
enum Snapshot {
    Scene(floptle_scene::SceneDoc),
    /// A terrain stroke snapshot: `(terrain id, the touched chunks' pre-stroke
    /// contents)` — keyed by the stable id (not Entity) so it survives scene
    /// restores. Undo/redo swaps the chunks back through the live field.
    Terrain(u32, floptle_field::ChunkUndo),
    /// A vertex-paint snapshot: `(paint id, colors per part)`. Keyed by the stable
    /// paint id for the same reason terrain is — `restore()` respawns the World, so an
    /// Entity here would dangle. Undo/redo is a colors swap that never touches the ECS.
    VertexPaint(u32, Vec<Vec<[u8; 4]>>),
    /// A texture-paint stroke: per touched node, `(tex-paint id, pre-stroke images per
    /// part)`. `None` images = that node had no paint before this stroke, so undo REMOVES
    /// its paint. A Vec because the world-space brush sphere paints EVERY surface it
    /// touches (that's how you shade a wall-floor corner in one stroke) — and one stroke
    /// must be one undo step, however many nodes it crossed. Keyed by the stable id.
    TexPaint(Vec<(u32, Option<Vec<Vec<u8>>>)>),
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

/// Which gizmo categories draw while the master ◎ toggle is on — the ⏷ menu
/// beside it. Everything defaults ON; the filter narrows, never adds.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct GizmoFilter {
    /// Camera frusta.
    pub(crate) cameras: bool,
    /// Point-light ranges, gravity volumes, the sun-direction arrow.
    pub(crate) lights: bool,
    /// Rigidbody outlines + contact crosses.
    pub(crate) physics: bool,
    /// Terrain / mesh / primitive collider wireframes.
    pub(crate) colliders: bool,
    /// Particle emitter shapes + force arrows.
    pub(crate) particles: bool,
    /// Lua `gizmo.*` debug draws.
    pub(crate) script: bool,
}

impl Default for GizmoFilter {
    fn default() -> Self {
        Self { cameras: true, lights: true, physics: true, colliders: true, particles: true, script: true }
    }
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
        // A project path from the CLI (the Hub launches `floptle-editor <project>`) wins;
        // otherwise default to the repo's `assets/` folder. File ⏵ Open/New re-points it.
        if self.project_root.as_os_str().is_empty() {
            self.project_root = PathBuf::from("assets");
        }
        self.dock_state = Some(default_dock());
        self.viewport_zoom = 0.9;
        self.terrain_voxel = 1.5;
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
        let title = if self.player_mode {
            if self.game_title.is_empty() {
                // Fall back to the project folder's name.
                self.project_root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Floptle Game".into())
            } else {
                self.game_title.clone()
            }
        } else {
            "Floptle Editor".into()
        };
        let attrs = Window::default_attributes()
            .with_title(&title)
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));
        let gpu = Gpu::new(window.clone());
        let mut raster = Raster::new(&gpu);
        // Registration order defines the Shape→MeshId mapping (Shape as usize):
        // Cube=0, Sphere=1, Capsule=2, Plane=3.
        // Geometry comes from matter_catalog::primitive_mesh so the paint brush's CPU
        // cache raycasts the EXACT mesh drawn here — paint is indexed by vertex_index,
        // so a divergence would paint the wrong vertices.
        use crate::matter_catalog::primitive_mesh;
        use floptle_core::Shape;
        let cube_id = raster.register(&gpu, &primitive_mesh(Shape::Cube), None);
        let sphere_id = raster.register(&gpu, &primitive_mesh(Shape::Sphere), None);
        let capsule_id = raster.register(&gpu, &primitive_mesh(Shape::Capsule), None);
        let plane_id = raster.register(&gpu, &primitive_mesh(Shape::Plane), None);
        self.mesh_ids = vec![cube_id, sphere_id, capsule_id, plane_id];
        self.raymarch = Some(Raymarch::new(&gpu));

        // Built-in primitive meshes for particle mesh-render tracks (see vfx.rs). Reserved
        // `builtin://…` keys in mesh_registry so the VFX picker offers stock shapes and
        // resolve_mesh_particles finds them by key like any imported model.
        for (key, _) in crate::vfx::BUILTIN_PARTICLE_MESHES {
            if let Some(data) = crate::vfx::builtin_particle_mesh_data(key) {
                let id = raster.register(&gpu, &data, None);
                self.mesh_registry
                    .insert((*key).to_string(), MeshAsset { parts: vec![id], size: 1.0, rig: None });
            }
        }

        // Seed the project folder structure + default assets, then load the scene,
        // project settings, materials and asset tree from `project_root`.
        self.seed_project_dirs();
        let (scene_file, doc) = self.load_active_scene();
        self.set_scene_file(&scene_file);
        floptle_scene::spawn_into(&doc, &mut self.world);
        self.adopt_terrain();
        // NOTE: adopt_paint/adopt_tex_paint happen AFTER `self.gpu = Some(..)` below —
        // both allocate GPU blocks/textures, and at this point gpu/raster are still
        // locals. Calling them here silently no-ops and boot loses all saved paint.
        if !self.player_mode {
            self.check_autosave(); // offer crash recovery if an autosave is newer
        }
        self.project = floptle_scene::load_project(&self.project_cfg_path());
        self.migrate_legacy_post(&doc);
        self.asset_tree = build_assets(&self.project_root);
        self.materials = self.load_materials();
        self.anim.rescan(&self.project_root);
        self.vfx.rescan(&self.project_root);
        self.load_texture_settings();

        self.retro = Some(Retro::new(&gpu, self.project.retro_height.max(80)));
        self.post = Some(floptle_render::PostStack::new(&gpu, gpu.config.width, gpu.config.height));
        self.outline = Some(Outline::new(&gpu));
        self.grid_render = Some(Grid::new(&gpu));
        self.particles = Some(floptle_render::Particles::new(&gpu));
        self.ui_render = Some(floptle_render::Ui::new(&gpu));

        let ctx = egui::Context::default();
        // egui's proportional fallback (Ubuntu + the two emoji fonts) is
        // missing many of the arrow/geometry glyphs the editor uses as icons
        // (→ ● ◌ ⊘ ⊕ …); Hack covers them and already ships with egui, so
        // append it or those labels render as tofu squares.
        let mut fonts = egui::FontDefinitions::default();
        if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            fam.push("Hack".into());
        }
        ctx.set_fonts(fonts);
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
        // Saved paint comes back only now that gpu/raster live in `self` (vertex blocks +
        // texture-paint atlases are GPU allocations — see the NOTE at the scene load above).
        self.adopt_paint();
        self.adopt_tex_paint();
        let now = Instant::now();
        self.last = Some(now);
        self.started = Some(now);
        self.window = Some(window);
        // Player mode boots straight into the game: Game view fullscreen (no
        // dock chrome renders around it) and Play running from frame one.
        if self.player_mode {
            self.fullscreen_tab = Some(EditorTab::Game);
            self.toggle_play();
            self.console.push(
                floptle_script::LogLevel::Debug,
                "🎮 player mode — F1 opens the multiplayer menu".into(),
                None,
            );
        }
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
                if self.scene_dirty && !self.player_mode {
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
                    // Fly-camera keys arm while the pointer is over the Scene
                    // viewport OR while RMB mouse-look is active — WASD in the
                    // Animating tab (or any other panel) must not drive the editor
                    // camera. The `looking` clause is load-bearing: entering look
                    // grabs+hides the cursor and nulls `self.cursor`, so
                    // `cursor_over_scene()` can no longer see it. Without it the
                    // classic hold-RMB + WASD fly combo is impossible and the two
                    // inputs silently cancel each other (the "camera freezes" bug).
                    let mv =
                        pressed && !typing && !game_view && (self.input.looking || self.cursor_over_scene());
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
                    // mode regardless of which panel has focus). Edges land in BOTH
                    // the per-frame sets (for `update`) and the per-tick accumulators
                    // (for `fixedUpdate` — consumed tick by tick, never lost).
                    if let Some(name) = key_name(code) {
                        if pressed {
                            if self.input_keys.insert(name.to_string()) {
                                self.input_keys_pressed.insert(name.to_string());
                                self.tick_keys_pressed.insert(name.to_string());
                            }
                        } else if self.input_keys.remove(name) {
                            self.input_keys_released.insert(name.to_string());
                            self.tick_keys_released.insert(name.to_string());
                        }
                    }
                    // Discrete commands fire on press only.
                    if pressed && !typing {
                        // Engine controls work in any view (Play/Pause/Quit).
                        match code {
                            KeyCode::Escape => {
                                // Escape is a "cancel" gesture first: free a trapped Game
                                // cursor, back out of an in-progress transition drag or the
                                // graph window, and never silently discard unsaved work.
                                // A BUILD (player mode) only ever frees the cursor — games
                                // don't quit on Escape.
                                if self.game_trap {
                                    self.game_trap = false;
                                    if let Some(window) = self.window.as_ref() {
                                        self.cursor_lock_soft = grab_cursor(window, false);
                                    }
                                } else if self.player_mode {
                                    // nothing else to cancel in a build
                                } else if self.anim_ui.drag_from.is_some() {
                                    self.anim_ui.drag_from = None;
                                } else if self.scene_dirty {
                                    self.show_quit_confirm = true;
                                } else {
                                    event_loop.exit();
                                }
                            }
                            // In a build, Play IS the program — F1 opens the
                            // multiplayer menu instead, and pause is editor-only.
                            KeyCode::F1 if self.player_mode => {
                                self.show_net_panel = !self.show_net_panel;
                            }
                            KeyCode::F1 => self.toggle_play(),
                            KeyCode::F2 if self.player_mode => {}
                            KeyCode::F2 => self.toggle_pause(),
                            // Everything else is an EDITOR shortcut — suppressed in the
                            // Game view so it behaves like a real build.
                            _ if !game_view => {
                                // A focused timeline tab (Animating/Graph/Particles) OWNS
                                // Delete, the arrows, F, Space, Home/End for its own
                                // keyframes/events — so suppress the scene versions here,
                                // letting the panel's own egui handlers run. App-wide
                                // controls (undo/redo/save) still fire everywhere.
                                let in_timeline = matches!(
                                    self.focused_tab,
                                    Some(
                                        EditorTab::Animation
                                            | EditorTab::AnimGraph
                                            | EditorTab::Particles
                                            | EditorTab::ShaderGraph
                                    )
                                );
                                // The ◈ Shaders canvas has its own undo stack
                                // (printed sources) — scene undo stays out.
                                let in_graph =
                                    matches!(self.focused_tab, Some(EditorTab::ShaderGraph));
                                if self.ctrl {
                                    match code {
                                        KeyCode::KeyZ if !in_graph => self.undo(),
                                        KeyCode::KeyY if !in_graph => self.redo(),
                                        KeyCode::KeyS => self.save_all(),
                                        // Scene-mutating — not while a timeline has focus.
                                        KeyCode::KeyC if !in_timeline => self.copy_selected(),
                                        KeyCode::KeyV if !in_timeline => self.paste(),
                                        KeyCode::KeyD if !in_timeline => self.duplicate_selected(),
                                        KeyCode::KeyA if !in_timeline => self.select_all(),
                                        _ => {}
                                    }
                                } else if !in_timeline {
                                    match code {
                                        KeyCode::Delete | KeyCode::Backspace => self.delete_selected(),
                                        KeyCode::KeyF => self.focus_selected(),
                                        KeyCode::KeyQ => self.selection.clear(), // unselect
                                        KeyCode::KeyG => self.grid.show = !self.grid.show, // toggle grid
                                        // Gizmos master toggle — H, beside G like the grid.
                                        KeyCode::KeyH => self.show_gizmos = !self.show_gizmos,
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
                    // Clicking into the Game view while playing traps the cursor there
                    // (Escape or Stop releases it) so playing doesn't let the mouse
                    // wander onto editor panels. `cursor_over_game()` gates it to the
                    // Game rect, so a click on any panel never grabs.
                    if self.playing && !self.game_trap && self.cursor_over_game() {
                        self.game_trap = true;
                        if let Some(window) = self.window.as_ref() {
                            self.cursor_lock_soft = grab_cursor(window, true);
                        }
                        self.cursor = None;
                    }
                    // Clicking anywhere outside a text field ends text editing —
                    // a click into the viewport (which egui never sees) included.
                    if let Some(eg) = self.egui.as_ref()
                        && !eg.ctx.is_pointer_over_egui()
                            && let Some(f) = eg.ctx.memory(|m| m.focused()) {
                                eg.ctx.memory_mut(|m| m.surrender_focus(f));
                            }
                    // In the Game view a left click is a GAME input only — never an editor
                    // pick/sculpt/gizmo-grab (it plays like a build), so treat it as not
                    // over the scene for editor purposes.
                    let over_scene = self.cursor_over_scene() && !self.game_view();
                    let hovered = self.gizmo.as_ref().and_then(|g| g.hovered);
                    if over_scene && self.tool == Tool::Paint && !self.playing {
                        // Paint tool takes the WHOLE click — no pick, no gizmo grab.
                        // The dab lands next frame in vertex_paint_frame_update, once
                        // the cursor ray has told us which node is under it.
                        self.context_menu = None;
                        self.painting = true;
                        self.last_dab_pos = None; // first dab fires immediately
                        self.last_dab_time = None;
                        self.paint_stroke_snapshot = None;
                        self.paint_stroke_dabbed = false;
                    } else if over_scene && self.tool == Tool::Sculpt {
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
                        if self.ui_overlay_hot {
                            // On a UI-overlay interact (element rect / Rect handle):
                            // egui owns this press — selecting or dragging happens
                            // there. Picking here would miss (elements are 2D) and
                            // clear the selection, killing the handle mid-grab.
                        } else if let (Some(h), Some(e)) = (hovered, self.primary()) {
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
                    // End of a paint stroke: bank the whole stroke as ONE undo step.
                    if self.painting {
                        self.painting = false;
                        self.end_paint_stroke();
                    }
                    // End of a sculpt stroke: bank one undo step if it changed anything,
                    // and re-derive the shadow proxy if the stroke outgrew its box.
                    if let Some((id, snap)) = self.stroke_snapshot.take()
                        && self.stroke_dabbed {
                            self.push_history(Snapshot::Terrain(id, snap));
                            self.end_sculpt_stroke();
                        }
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Middle, .. } => {
                let pressed = state == ElementState::Pressed;
                self.track_mouse_button(2, pressed);
                // MMB drag over the Scene viewport pans the fly camera. Grab the cursor
                // (raw delta, so panning never freezes at a window edge) and restore it
                // to the press point on release. Editor Scene view only.
                let editor_scene = !self.game_view() && self.cursor_over_scene();
                if pressed && editor_scene {
                    self.panning = true;
                    self.pan_press = self.cursor;
                    if let Some(window) = self.window.as_ref() {
                        self.cursor_lock_soft = grab_cursor(window, true);
                    }
                    self.cursor = None;
                } else if !pressed && self.panning {
                    self.panning = false;
                    if !self.script_mouse_lock && !self.input.looking && !self.game_trap
                        && let Some(window) = self.window.as_ref()
                    {
                        self.cursor_lock_soft = grab_cursor(window, false);
                    }
                    self.cursor = self.pan_press.take().or(self.cursor);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let d = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y,
                    winit::event::MouseScrollDelta::PixelDelta(p) => p.y as f32 / 40.0,
                };
                self.input_scroll += d;
                self.tick_scroll += d;
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
                    // Don't release the grab if a script is holding the mouse locked, the
                    // Game view has it trapped, or an MMB pan is still dragging.
                    if !self.script_mouse_lock && !self.game_trap && !self.panning
                        && let Some(window) = self.window.as_ref() {
                            self.cursor_lock_soft = grab_cursor(window, false);
                        }
                    // A click (negligible motion) over the viewport ⏵ context menu (editor only).
                    if editor && was_looking && self.rmb_moved < 6.0
                        && let Some(p) = self.rmb_press {
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
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            // Accumulate raw mouse delta for the script `input` API (frame + tick).
            self.input_mouse_delta.0 += delta.0 as f32;
            self.input_mouse_delta.1 += delta.1 as f32;
            self.tick_mouse_delta.0 += delta.0 as f32;
            self.tick_mouse_delta.1 += delta.1 as f32;
            // Priority: RMB-look > MMB-pan > grabbed gizmo handle. (Free dragging an
            // object now requires the Move tool's center handle — no accidental moves.)
            if self.input.looking {
                self.camera.look(delta.0 as f32, delta.1 as f32);
                self.rmb_moved += (delta.0.abs() + delta.1.abs()) as f32;
            } else if self.panning {
                self.camera.pan(delta.0 as f32, delta.1 as f32);
            } else if self.grabbed.is_some() {
                self.gizmo_drag();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // A quit-modal decision (Save & Quit / Quit without saving) sets `pending_exit`;
        // the save already ran during the frame, so now actually leave. Doing it here (not
        // inside the egui closure, which has no `event_loop`) is what makes the button close
        // the app for real.
        if self.pending_exit {
            event_loop.exit();
            return;
        }
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }
}

#[cfg(test)]
mod cli_tests {
    use super::json_string_field;

    #[test]
    fn reads_version_from_bundle_json() {
        let json = r#"{ "version": "0.1.0", "target": "linux-x86_64", "commit": "abc1234" }"#;
        assert_eq!(json_string_field(json, "version").as_deref(), Some("0.1.0"));
        assert_eq!(json_string_field(json, "target").as_deref(), Some("linux-x86_64"));
        // Whitespace-tolerant and prerelease-safe.
        assert_eq!(
            json_string_field("{\n  \"version\"  :   \"1.2.0-rc.3\"\n}", "version").as_deref(),
            Some("1.2.0-rc.3")
        );
    }

    #[test]
    fn missing_or_malformed_fields_are_none() {
        assert_eq!(json_string_field(r#"{ "target": "x" }"#, "version"), None);
        assert_eq!(json_string_field("{ \"version\": 3 }", "version"), None); // not a string
        assert_eq!(json_string_field("", "version"), None);
    }
}

#[cfg(test)]
mod export_tests {
    use super::{export_game_with, Editor, GameManifest, EXPORT_TARGETS};
    use std::path::PathBuf;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("floptle-export-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A typed export folder resolves PREDICTABLY: absolute stays put, relative
    /// lands next to the project (its parent) — never the process CWD, which
    /// depends on how the editor was launched (Ty's "where do paths actually
    /// reference" complaint).
    #[test]
    fn export_dir_resolves_against_the_project_parent() {
        let mut ed = Editor { project_root: PathBuf::from("/repo/assets"), ..Default::default() };
        assert_eq!(ed.resolve_export_dir("builds"), PathBuf::from("/repo/builds"));
        assert_eq!(ed.resolve_export_dir("/abs/dist"), PathBuf::from("/abs/dist"));
        ed.project_root = PathBuf::from("/");
        assert_eq!(ed.resolve_export_dir("b"), PathBuf::from("/b"));
    }

    /// Export = binary + assets (dot-entries skipped) + a manifest that parses
    /// back and points at the copied project.
    #[test]
    fn export_stamps_a_runnable_build() {
        let proj = temp("proj");
        std::fs::create_dir_all(proj.join("scenes")).unwrap();
        std::fs::write(proj.join("project.ron"), "()").unwrap();
        std::fs::write(proj.join("scenes/first.ron"), "()").unwrap();
        std::fs::create_dir_all(proj.join(".floptle")).unwrap();
        std::fs::write(proj.join(".floptle/cache.bin"), "x").unwrap();
        std::fs::write(proj.join(".luarc.json"), "{}").unwrap();
        let out = temp("out");

        let me = std::env::current_exe().unwrap();
        let (msg, done_dir) = export_game_with(&proj, &out, "My Cool Game!", &me, &EXPORT_TARGETS[0])
            .expect("export succeeds");
        assert!(msg.contains("2 asset file(s)"), "dot-entries must be skipped: {msg}");
        assert!(done_dir.is_dir(), "the success result carries the build folder");
        assert!(out.join("assets/project.ron").is_file());
        assert!(out.join("assets/scenes/first.ron").is_file());
        assert!(!out.join("assets/.floptle").exists(), "editor cache must not ship");
        // The binary landed under a filesystem-safe name (this test binary).
        let exe = format!("My_Cool_Game{}", std::env::consts::EXE_SUFFIX);
        assert!(out.join(&exe).is_file(), "missing {exe}");
        let manifest: GameManifest =
            ron::from_str(&std::fs::read_to_string(out.join("floptle-game.ron")).unwrap())
                .expect("manifest parses");
        assert_eq!(manifest.title, "My Cool Game!");
        assert_eq!(manifest.project, "assets");

        // Exporting INTO the project is refused (it would copy itself).
        let inside = proj.join("build");
        assert!(export_game_with(&proj, &inside, "x", &me, &EXPORT_TARGETS[0]).is_err());

        // A macOS-target export ships the Gatekeeper README, {exe} filled in.
        let out2 = temp("out-mac");
        let mac = EXPORT_TARGETS.iter().find(|t| t.label == "macOS").unwrap();
        export_game_with(&proj, &out2, "Sea Game", &me, mac).expect("mac export");
        let readme = std::fs::read_to_string(out2.join("README.txt")).unwrap();
        assert!(readme.contains("./Sea_Game"), "README names the actual binary: {readme}");
        assert!(readme.contains("com.apple.quarantine"));
        let _ = std::fs::remove_dir_all(&out2);

        let _ = std::fs::remove_dir_all(&proj);
        let _ = std::fs::remove_dir_all(&out);
    }

    /// The trap behind "the build opens the editor": a project rooted at
    /// `assets/` exported with the default title on a suffix-less target named
    /// the exe `assets` — colliding with the shipped assets FOLDER, killing the
    /// export halfway and leaving a manifest-less binary that boots as the
    /// editor. The exe must dodge the reserved name, a re-export must clean up
    /// the old broken artifact, and the binary must ship LAST so a failed
    /// export never leaves anything runnable.
    #[test]
    fn export_never_collides_the_exe_with_the_assets_folder() {
        let proj = temp("proj-collide");
        std::fs::write(proj.join("project.ron"), "()").unwrap();
        let out = temp("out-collide");
        // Ty's exact broken state: a previous half-export left a FILE named
        // `assets` (the binary) in the build folder.
        std::fs::write(out.join("assets"), "old broken binary").unwrap();

        let me = std::env::current_exe().unwrap();
        let bare = super::ExportTarget {
            label: "test",
            kind: super::ExportKind::SelfBinary,
            exe_suffix: "", // suffix-less target = the collision case
            readme: None,
        };
        export_game_with(&proj, &out, "assets", &me, &bare).expect("collision export succeeds");
        assert!(out.join("assets").is_dir(), "assets must be the project folder, not the exe");
        assert!(out.join("game").is_file(), "the exe dodges the reserved name");
        assert!(out.join("floptle-game.ron").is_file(), "the build is a GAME (manifest present)");

        // Binary ships LAST: a copy failure (bogus source binary) errors out
        // without leaving a runnable exe in the build folder.
        let out2 = temp("out-nobin");
        let err = export_game_with(&proj, &out2, "Cool", &me.join("nope"), &bare)
            .expect_err("bogus binary must fail");
        assert!(err.contains("copy binary"), "fails at the binary step: {err}");
        assert!(!out2.join("Cool").exists(), "no runnable exe after a failed export");

        let _ = std::fs::remove_dir_all(&proj);
        let _ = std::fs::remove_dir_all(&out);
        let _ = std::fs::remove_dir_all(&out2);
    }
}

