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
    capsule, cube, uv_sphere, FlyCamera, Gpu, Grid, Input, MeshId, Outline, Raster, Raymarch, Retro, TexId,
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
mod assets_ui;
mod console;
mod dock;
mod gizmo;
mod hierarchy;
mod history;
mod ide;
mod inspector;
mod lua_support;
mod matter_catalog;
mod play;
mod prefs;
mod project;
mod render_frame;
mod scene_ops;
mod scene_tab;
mod selection;
mod shading;
mod terrain_edit;
mod terrain_ui;
mod theme;
mod vfx;
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
use terrain_ui::*;
use theme::*;
use viz::*;

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
    /// Attach a ParticleSystem component referencing an existing effect asset.
    add_particles: Option<(Entity, String)>,
    /// Create a starter `.vfx.ron` effect and attach it to this entity.
    new_particles: Option<Entity>,
    remove_particles: Option<Entity>,
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
    /// Particle effect registry (the inspector lists its keys).
    vfx: &'a mut vfx::VfxSystem,
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

fn main() {
    env_logger::init();
    println!("{} editor v{}", floptle_core::ENGINE_NAME, floptle_core::ENGINE_VERSION);
    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    // Gizmos/overlays on by default (toggle in the viewport).
    let mut editor = Editor { show_gizmos: true, ..Default::default() };
    event_loop.run_app(&mut editor).expect("run editor");
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
    /// Particles: effect registry + live play-mode instances.
    vfx: vfx::VfxSystem,
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
        self.vfx.rescan(&self.project_root);
        self.load_texture_settings();

        self.retro = Some(Retro::new(&gpu, self.project.retro_height.max(80)));
        self.post = Some(floptle_render::PostStack::new(&gpu, gpu.config.width, gpu.config.height));
        self.outline = Some(Outline::new(&gpu));
        self.grid_render = Some(Grid::new(&gpu));
        self.particles = Some(floptle_render::Particles::new(&gpu));

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
                    if let Some((id, snap)) = self.stroke_snapshot.take()
                        && self.stroke_dabbed {
                            self.push_history(Snapshot::Terrain(id, snap));
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
                    if !self.script_mouse_lock
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

