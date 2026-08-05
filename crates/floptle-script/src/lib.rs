//! # floptle-script — the Lua scripting host (ADR-0003)
//!
//! Game logic lives in `.lua` files under a project's `scripts/` folder, attached
//! to nodes (the [`floptle_core::Scripts`] component names which scripts run, with
//! per-instance float `params`). [`ScriptHost`] embeds Lua (LuaJIT via `mlua`) and
//! drives them each frame.
//!
//! ## The script contract
//! A script file defines plain functions in its own sandboxed environment:
//! ```lua
//! defaults = { speed = 45 }              -- tunables shown in the Inspector
//!
//! function start(node) end               -- once, when play begins (optional)
//!
//! function update(node, dt)              -- every frame while playing
//!   node.yaw = node.yaw + math.rad(params.speed) * dt
//! end
//!
//! function fixedUpdate(node, dt)         -- every GAMEPLAY TICK (constant dt)
//!   -- movement / gameplay / physics writes belong here (netcode cadence)
//! end
//! ```
//! The host hands each call a mutable `node` table (`x/y/z`, `scale`/`scale_x..z`,
//! `yaw/pitch/roll` in radians) synced to the node's [`Transform`] before the call
//! and read back after, plus the globals `params` (this instance's values), `time`
//! (seconds since play started) and `dt`. The full Lua standard library is in
//! scope; `log("...")` prints to the engine console.
//!
//! Each `(node, script)` pair gets its own environment so per-instance state
//! persists across frames, and the host **hot-reloads** a script when its file
//! changes on disk (re-running it in a fresh environment).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::SystemTime;

use floptle_core::transform::Transform;
use floptle_core::{Entity, Material};
use mlua::{Lua, RegistryKey, Table};

/// Queued `node:setShaderParam(...)` writes: (entity index, uniform name, vec4 lanes).
type ShaderParamSets = Rc<RefCell<Vec<(u32, String, [f32; 4])>>>;

/// `(script key name, why the host keeps it)` — see [`ScriptHost::set_reserved_keys`].
type ReservedKeys = Rc<RefCell<Vec<(String, String)>>>;

/// The frame profile, shared between the driver, the Lua `perf` table and the
/// editor readout (`floptle/0077`).
pub type SharedProfile = Rc<RefCell<floptle_core::profile::FrameProfile>>;

/// One world-space line segment a script queued via `draw.line(...)` this tick
/// (immediate mode — re-queued every tick while wanted). Drawn depth-tested by
/// the runtime line layer; the S6 v2 map draws its orbit conics with these.
#[derive(Clone, Copy, Debug)]
pub struct DrawLine {
    pub a: [f64; 3],
    pub b: [f64; 3],
    pub color: [f32; 4],
}

/// One SCREEN-SPACE rectangle a script queued via `draw.rect` /
/// `draw.rectOutline` this tick (immediate mode, like the 3D `draw.*` calls).
///
/// Pixels are the same space `input.mouse()` and `camera.worldToScreen` use, so
/// a marquee is literally "the rect between where I pressed and where the cursor
/// is" — no projection, no ground plane, no camera angle to fight. Drawn through
/// the game-UI pipeline, over everything, in the Game view and in a build alike.
#[derive(Clone, Copy, Debug)]
pub struct DrawRect {
    /// `[x, y, w, h]` in physical pixels.
    pub rect: [f32; 4],
    pub color: [f32; 4],
    /// Border width in px — `0` fills the rect instead of outlining it.
    pub outline: f32,
    /// Corner radius in px.
    pub radius: f32,
}

/// One screen-space string a script queued via `draw.text` this tick.
///
/// A separate queue from [`DrawRect`] because text has to reach the glyph
/// layout the UI renderer already owns — the script says *what* and *where*,
/// and never has to know how wide an 'm' is.
#[derive(Clone, Debug)]
pub struct DrawText {
    /// Top-left in physical pixels — the same space `input.mouse()` reports.
    pub pos: [f32; 2],
    pub text: String,
    pub size: f32,
    pub color: [f32; 4],
    /// `0` left (x is the left edge), `1` centre, `2` right — the alignment
    /// that makes a right-hand HUD column line up without measuring anything.
    pub align: u8,
}

/// One world-space FILLED triangle a script queued via `draw.tri` / `draw.cone`
/// / `draw.disc` this tick (immediate mode). Drawn by the runtime triangle
/// layer alongside the lines — solid gizmo geometry, world markers.
#[derive(Clone, Copy, Debug)]
pub struct DrawTri {
    pub a: [f64; 3],
    pub b: [f64; 3],
    pub c: [f64; 3],
    pub color: [f32; 4],
}

/// Queued `node:getcomponent(name).field = value` writes: (entity index,
/// component, field) → value, flushed to the ECS after `run`.
///
/// DETERMINISM INVARIANT (audited 2026-07-06, `docs/netcode-design.md` §3): the
/// host's `HashMap`/`HashSet` state is only ever *iterated* where order cannot
/// change simulation results — each queued write lands on a distinct key
/// (entity/component/field), scripts themselves run in ECS insertion order
/// (a `Vec` snapshot), and the `input` sets are lookup-only. Keep it that way:
/// if a future queue's application order can affect the sim, use a `Vec` or
/// sort before applying — netcode prediction replays depend on same-inputs →
/// same-results.
type ComponentWrites = Rc<RefCell<HashMap<(u32, String, String), f64>>>;
/// The COLOUR-valued twin of [`ComponentWrites`] (`e.fill = color(...)`). A
/// separate map because `borderR` already means the right border width — one
/// namespace would have made a colour assignment resize an edge.
type ComponentColorWrites = Rc<RefCell<HashMap<(u32, String, String), [f32; 4]>>>;
/// `node:getcomponent(...).field = "some/path.png"` writes: the string-valued
/// counterpart of [`ComponentWrites`], for the fields a number cannot express
/// (a UI image's texture, a Material's texture, a text element's string).
type ComponentStrWrites = Rc<RefCell<HashMap<(u32, String, String), String>>>;

/// One live `ui.bind(node, prop, fn)`: the engine calls `fn` once a frame and
/// writes what it returns.
///
/// This exists because "keep this label showing that number" was an `update`
/// per label — every one of them a place to forget the formatting, drift out
/// of step, or keep writing after the panel closed. The binding says the
/// relationship once.
pub(crate) struct UiBinding {
    pub e: u32,
    pub prop: String,
    pub f: mlua::RegistryKey,
}

type UiBindings = Rc<RefCell<Vec<UiBinding>>>;

/// One queued `scene.*` transition, drained by the driver between frames.
///
/// A LIST rather than a single slot, because additive loads compose: a level
/// that brings in its terrain, its props and its music in one `start` is three
/// requests and all three must happen. A full swap is still last-one-wins —
/// the driver stops at the first one it performs, since everything queued
/// behind it named the world that just stopped existing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SceneRequest {
    /// `scene.load(name)` — replace the world.
    Load { name: String },
    /// `scene.load(name, { additive = true })` — layer on top of it.
    ///
    /// `environment` is `{ environment = true }`: the layer OWNS the world's
    /// environment while it is loaded — its `lighting` block (sun + fog) plus
    /// its Skybox and PostProcess nodes replace the base scene's, which are
    /// disabled rather than destroyed and come back on `unload`. Without it an
    /// additive layer brings nodes only, and a second Skybox would leave the
    /// look decided by query order.
    Additive { name: String, environment: bool },
    /// `scene.unload(name)` — take an additive layer away again.
    Unload { name: String },
}

impl SceneRequest {
    /// The scene this request names, whichever kind it is.
    pub fn name(&self) -> &str {
        match self {
            SceneRequest::Load { name }
            | SceneRequest::Additive { name, .. }
            | SceneRequest::Unload { name } => name,
        }
    }
    /// True for the kind that replaces the world (the one a session must
    /// announce, and the one that ends everything queued behind it).
    pub fn is_swap(&self) -> bool {
        matches!(self, SceneRequest::Load { .. })
    }
}

type SceneQueue = Rc<RefCell<Vec<SceneRequest>>>;

/// Queued `ui.make(container, tree)` calls, drained by the driver.
type UiMakes = Rc<RefCell<Vec<ui_make::MakeRequest>>>;

/// Behaviour closures a made element carries (`onClicked` and friends), by
/// `(entity, hook)`. Kept beside the scripts rather than inside them: the
/// element has no script file to put a `clicked` function in, which is the
/// whole point of describing a screen in one place.
type UiHandlers = Rc<RefCell<HashMap<(u32, String), mlua::RegistryKey>>>;

/// One live `ui.on(element, hook, fn)`: a script listening to an element it
/// does not live on.
///
/// The owner is the LISTENING script, not the element — which is the whole
/// point. A menu manager holds every button's `clicked` in one file, instead of
/// a three-line script per button, and its listeners live and die with it: a
/// reload re-registers them, and destroying the manager stops them.
pub(crate) struct UiListener {
    /// The element being listened to.
    pub e: u32,
    /// Which hook (`"clicked"`, `"changed"`, … — [`ui_make::HOOKS`]).
    pub hook: String,
    /// The `(entity, script kind)` that registered it.
    pub owner: (u32, String),
    pub f: mlua::RegistryKey,
}

type UiListeners = Rc<RefCell<Vec<UiListener>>>;

/// This frame's UI interaction events (`(element, hook)`), fed by the engine
/// BEFORE the scripts run — what `ui.clicked(el)` and `ui.events()` read.
///
/// The same list the engine dispatches hooks from afterwards, published early
/// so a script that would rather ASK than be called back gets this frame's
/// answer rather than last frame's.
type UiFrameEvents = Rc<RefCell<Vec<(u32, String)>>>;

mod account_api;
mod api;
mod audio_api;
mod env;
mod host;
mod http_api;
pub use http_api::open_in_browser;
mod input_api;
mod math_api;
pub mod access_api;
mod net_api;
pub mod opts;
mod perf_api;
pub mod rollback_api;
mod preprocess;
mod save_api;
mod scatter_api;
mod sched_api;
mod shape_api;
mod assembly_api;
mod space_api;
mod terrain_api;
pub mod ui_make;
mod view_api;
pub mod water_api;

pub(crate) use api::install_handle_api;
/// Live ECS field appliers, reused by the animation system's property tracks.
/// `mirror_components` reads them back (numeric) — the animation recorder diffs
/// it to auto-key changed properties.
pub use api::{
    apply_component_color, apply_component_field, apply_component_field_str,
    mirror_component_colors, mirror_components, HANDLE_KEYS,
};
pub use input_api::{SharedDomain, SharedInput};
pub use net_api::{
    input_to_net, net_aim, net_to_input, NetCmd, NetRoleState, NetState, RewindScope, RollbackInfo,
};
pub use assembly_api::{AssemblyCmd, AssemblyImpact, AssemblyInfo};
pub use space_api::{SpaceBodyInfo, SpaceInfo};
pub use terrain_api::{TerrainOp, TerrainOpMode, TerrainYield};
pub use rollback_api::{ScriptState, MAX_STATE_DEPTH};
pub use view_api::ViewInfo;

/// Severity of a captured script log line (the engine Console colors by this).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Warn,
    Error,
}

/// One line emitted by a running script — a `print`/`log` call or a raised error.
/// `source` is the originating `(script name, 1-based line)` when known, so the
/// editor's Console can jump to it.
#[derive(Clone, Debug)]
pub struct ScriptLog {
    pub level: LogLevel,
    pub msg: String,
    pub source: Option<(String, u32)>,
}

/// Parse the 1-based line out of an mlua error string (formatted `name:LINE: msg`).
fn error_line(msg: &str) -> u32 {
    msg.split(':').find_map(|s| s.trim().parse::<u32>().ok()).unwrap_or(0)
}

/// The UI drag reported to scripts: `(source element, drop target under it)`.
pub(crate) type UiDragCell = Rc<RefCell<Option<(u32, Option<u32>)>>>;

/// A snapshot of player input for one frame, fed to scripts via the `input` global
/// (so games can read the keyboard/mouse). Key names are lowercase
/// (`"w"`, `"space"`, `"left"`, `"escape"`, …). Mouse position is in pixels;
/// buttons are 0 = left, 1 = right, 2 = middle.
#[derive(Clone, Debug, Default)]
pub struct InputSnapshot {
    /// Keys currently held this frame.
    pub keys_down: std::collections::HashSet<String>,
    /// Keys that went down THIS frame (edge).
    pub keys_pressed: std::collections::HashSet<String>,
    /// Keys that went up THIS frame (edge).
    pub keys_released: std::collections::HashSet<String>,
    /// The CHARACTERS entered this frame, resolved by the OS keyboard layout,
    /// with a paste folded in.
    ///
    /// Not the same question as `keys_pressed`: that one is physical (`"q"` is
    /// the key where Q sits on a QWERTY board, which types `a` on AZERTY), and
    /// this is what the player meant to write. Polling keys to build a string
    /// gets the alphabet wrong for anyone whose keyboard isn't yours.
    pub typed: String,
    pub mouse: (f32, f32),
    pub mouse_delta: (f32, f32),
    pub scroll: f32,
    pub buttons_down: [bool; 3],
    pub buttons_pressed: [bool; 3],
    /// The ACTIVE camera's world (yaw, pitch), captured with the snapshot —
    /// `input.aimYaw()`/`aimPitch()`. This makes camera-relative movement
    /// deterministic under prediction: the view direction rides the input
    /// command, so the server and any replay use EXACTLY the angle the player
    /// saw (a local camera node can never match across machines).
    pub aim: Option<[f32; 2]>,
}

/// A script source file's reload state: a generation that bumps whenever the file
/// changes, plus the last error seen for the current generation (so a broken
/// script is compiled at most once per edit, not re-run every frame).
struct Source {
    generation: u64,
    mtime: Option<SystemTime>,
    error: Option<String>,
}

/// A live `(node, script)` environment — the Lua table the script's functions
/// close over, tagged with the source generation it was built from.
struct Instance {
    env: RegistryKey,
    generation: u64,
    started: bool,
    seen: bool,
    /// The `node` table this instance's hooks are handed, kept alive between hooks and
    /// re-stamped rather than rebuilt, so a handle a script stashed in `start()` keeps
    /// reading the live transform. `stamp` is what the engine last wrote into it, so a
    /// write made from outside a hook can be told apart from an untouched field.
    ///
    /// A `RegistryKey` and not a live `Table` for the same reason `env` is: a
    /// Table held from Rust costs a slot on mlua's bounded auxiliary ref stack,
    /// and one per instance put a hard ceiling of a few thousand scripted nodes
    /// on a scene — reached as a PANIC (`floptle/0069`).
    node: Option<(RegistryKey, crate::env::NodeStamp)>,
}

/// Embeds Lua and runs the scripts attached to a world's nodes.
pub struct ScriptHost {
    lua: Lua,
    sources: HashMap<String, Source>,
    instances: HashMap<(u32, String), Instance>,
    errors: Vec<String>,
    /// Captured `print`/`log` output (and errors) since the last drain — the editor
    /// Console reads these. Shared with the Lua `print`/`log` closures.
    logs: Rc<RefCell<Vec<ScriptLog>>>,
    /// This frame's player input, shared with the Lua `input` table's functions.
    input: Rc<RefCell<InputSnapshot>>,
    /// The action map + per-player resolved state, shared with the Lua action
    /// API (`input.action("Jump")`). The driver resolves into it each frame and
    /// tick; scripts read through it, and `input.consume` writes to it.
    input_sys: crate::input_api::SharedInput,
    /// Which domain the running pass reads: `fixedUpdate` sees the tick domain
    /// (the one with input history), `update` the frame domain. Flipped by
    /// [`ScriptHost::run_pass`] so a script never has to ask.
    input_domain: crate::input_api::SharedDomain,
    /// This frame's physics body state per entity index (velocity + grounded), fed in
    /// before `run` so scripts can read `node.vx/vy/vz/grounded`.
    bodies: Rc<RefCell<HashMap<u32, BodyState>>>,
    /// This frame's solved UI element rects in WINDOW physical pixels (entity
    /// index → [x, y, w, h]); `node:uiRect()` reads it so scripts can hit-test
    /// the mouse against a panel's ACTUAL rendered position instead of guessing
    /// its geometry. Same space as `input.mouse()`, which is the only reason
    /// the comparison works.
    ui_rects: Rc<RefCell<HashMap<u32, [f32; 4]>>>,
    /// Velocities scripts wrote this frame (entity index → new velocity), drained by
    /// the editor and applied to the physics sim.
    body_changes: Rc<RefCell<HashMap<u32, [f32; 3]>>>,
    /// Capsule heights scripts wrote this frame (entity index → height), drained and
    /// applied to the sim — for crouching.
    body_height_changes: Rc<RefCell<HashMap<u32, f32>>>,
    /// Cross-node position writes on body entities → the driver teleports the
    /// body (see `Shared::body_pos_changes`).
    body_pos_changes: Rc<RefCell<HashMap<u32, [f64; 3]>>>,
    /// This frame's sprite-batch draws (see `Shared::sprite_draws`).
    sprite_draws: Rc<RefCell<HashMap<u32, Vec<floptle_core::Sprite>>>>,
    /// How many sprites the last flush wrote into the ECS, so a pass that drew
    /// nothing new skips the write — and the full-world scan that finds the
    /// batches. `None` at the frame boundary forces one write per frame even
    /// when the count is unchanged. See the flush in `host.rs`.
    sprites_written: Option<usize>,
    /// `node:setShaderParam(name, x, y, z, w)` writes — (entity index, uniform
    /// name, vec4 lanes), drained by the editor into the node's Material or UI
    /// ElementSpec `shader_params` (the per-frame shader drivers then upload).
    shader_param_sets: ShaderParamSets,
    /// The physics colliders for THIS frame, so `raycast(...)` works inside a script. The
    /// editor lends the sim's colliders before running scripts and takes them back after.
    colliders: Rc<RefCell<Vec<floptle_physics::AnchoredCollider>>>,
    /// Raycastable dynamic-body hulls for this frame ([`Sim::body_hulls`] copies —
    /// players, crates), fed alongside the colliders so `raycast(...)` can hit
    /// bodies AND name the node it hit (`hit.node`). `net.rewind` re-poses these
    /// for lag-compensated combat queries (`docs/netcode-design.md` §7).
    hulls: Rc<RefCell<Vec<floptle_physics::BodyHull>>>,
    /// World position of the sim's local origin (ADR-0015). Scripts speak world
    /// coordinates; `raycast` converts to the sim frame in f64 at this boundary.
    sim_origin: Rc<RefCell<glam::DVec3>>,
    /// Terrain edits queued by `terrain.sculpt/dig/paint(...)` this frame, drained by
    /// the editor after the script pass (applied to the authority field + sim copy).
    terrain_ops: Rc<RefCell<Vec<terrain_api::TerrainOp>>>,
    /// Measured yield reports posted back by the engine after ops are applied.
    terrain_yields: Rc<RefCell<Vec<terrain_api::TerrainYield>>>,
    /// `terrain.generatePlanet(id, opts)` requests — heavyweight whole-field
    /// generations the editor runs on a background thread.
    terrain_generates: Rc<RefCell<Vec<(u32, floptle_field::procgen::PlanetFill)>>>,
    /// `terrain.saveDir(path)` — the game's save-slot directory for player-
    /// edited terrain fields (G2). The residency streamer prefers fields here
    /// over project files / genspec regeneration, and writes evictions here.
    terrain_save_dir: Rc<RefCell<Option<String>>>,
    /// `terrain.warm(name)` requests this frame (immediate mode, drained per
    /// frame): body NAMES whose terrain should be resident regardless of any
    /// gameplay anchor's distance — the map warms its focused planet while
    /// open. A warmed body loads if cold and never evicts.
    terrain_warm: Rc<RefCell<Vec<String>>>,
    /// `terrain.flush()` — write every dirty resident field to the save slot
    /// NOW (checkpoints, exit-to-menu). One-shot flag drained per frame.
    terrain_flush: Rc<RefCell<bool>>,
    /// `createNode(...)` requests, drained with the spawn queue.
    create_requests: Rc<RefCell<Vec<CreateRequest>>>,
    /// Construction-API component/matter writes (see [`RichSet`]).
    rich_sets: Rc<RefCell<Vec<(u32, RichSet)>>>,
    /// The scene graph mirror the node handles read/write (synced each `run`).
    scene: Rc<RefCell<SceneMirror>>,
    /// Live per-(entity, script) environments, for script handles. Registry
    /// keys — see the note on the `Shared` copy of this field.
    envs: Rc<RefCell<HashMap<(u32, String), RegistryKey>>>,
    /// Mesh model paths scripts wrote this frame (entity index → new asset path), applied
    /// to the ECS `Matter::Mesh` in `run` and drained by the editor to re-import the GPU mesh.
    model_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// Material refs scripts assigned this frame (entity index → preset name / asset path),
    /// resolved against `materials` and applied to the ECS in `run`.
    material_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// `node.visible = ...` writes (entity index → shown), applied as a `Visible` component.
    visible_changes: Rc<RefCell<HashMap<u32, bool>>>,
    /// `node.enabled = …` — switches the node (and its subtree) off/on. Separate from
    /// `visible`: that one only stops the draw, this also stops physics and scripts.
    enabled_changes: Rc<RefCell<HashMap<u32, bool>>>,
    /// `node.persistent = …` — whether the node (and its subtree) survives a
    /// scene swap. Applied as a `Persistent` marker; absence means "ordinary".
    persistent_changes: Rc<RefCell<HashMap<u32, bool>>>,
    /// `node.layer = "Name"` writes (entity index → validated layer name),
    /// applied as a `Layer` component after `run` ("Default" removes it).
    layer_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// Tag edits: entity index → the node's full new tag list, applied as a
    /// `Tags` component after `run` (empty removes it).
    tag_changes: Rc<RefCell<HashMap<u32, Vec<String>>>>,
    /// The project's resolved layer table, set by the driver at Play start
    /// ([`Self::set_layers`]) — validates layer writes, resolves raycast masks.
    layer_table: Rc<RefCell<floptle_core::Layers>>,
    /// `node.text = ...` writes (entity index → text), applied to the node's UI ElementSpec.
    ui_text_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// `node.style = ...` writes (entity index → style name), applied to the
    /// node's UI ElementSpec. A separate channel from the text one because they
    /// are different fields that happen to share a "string, read-your-writes"
    /// shape; one map would have to tag every entry with which field it meant.
    ui_style_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// `node:getcomponent(name).field = value` writes, flushed to the ECS after `run`.
    component_changes: ComponentWrites,
    component_colors: ComponentColorWrites,
    component_strs: ComponentStrWrites,
    ui_bindings: UiBindings,
    /// `ui.make(...)` calls this pass, drained by the driver's spawn drain.
    ui_makes: UiMakes,
    /// The behaviour closures made elements carry.
    ui_handlers: UiHandlers,
    /// Live `ui.on(...)` listeners — scripts hearing about elements they don't
    /// live on.
    ui_listeners: UiListeners,
    /// Elements a listener was registered for since the last check, verified
    /// against the world in `run` (an element that takes no clicks would never
    /// fire, silently).
    ui_listener_checks: Rc<RefCell<Vec<(u32, String)>>>,
    /// This frame's `(element, hook)` events, for `ui.clicked(...)` / `ui.events()`.
    ui_frame_events: UiFrameEvents,
    /// The element under the pointer, fed by the engine each frame (`ui.hovered`).
    ui_hover: Rc<RefCell<Option<u32>>>,
    /// The element being held down, fed by the engine each frame (`ui.held`).
    ui_active: Rc<RefCell<Option<u32>>>,
    /// The material presets the editor lends each frame (name → Material), so a script can
    /// set `node.material = "Gold"` (or an `assets.getFile("materials/Gold.ron")`).
    materials: Rc<RefCell<HashMap<String, Material>>>,
    /// The project root, so `assets.getFile` / `assets.getContents` can resolve paths the
    /// dev writes relative to it (the `Assets/` folder). Set by the editor each frame.
    project_root: Rc<RefCell<PathBuf>>,
    /// The `save.*` persistent store (roadmap A2): per-slot key→NetValue map,
    /// lazily loaded, flushed by the editor on Stop + periodically during Play.
    save_state: Rc<RefCell<save_api::SaveState>>,
    /// The `after`/`every`/`tween` scheduler (roadmap A4). Tick-driven: advanced
    /// ONLY by the global `run_fixed` — never by `run_fixed_for`/replays, or
    /// prediction would double-fire every pending timer.
    sched: Rc<RefCell<sched_api::SchedState>>,
    /// This tick's celestial snapshot (`space.*` reads it; the editor feeds it).
    space_info: Rc<RefCell<space_api::SpaceInfo>>,
    /// This frame's active game camera + viewport (`camera.worldToScreen` reads
    /// it; the editor feeds it every frame). Powers map click-on-line picking.
    view_info: Rc<RefCell<view_api::ViewInfo>>,
    /// A pending `space.warp(m)` request the editor drains + applies.
    warp_request: Rc<RefCell<Option<f64>>>,
    /// A pending `physics.pause(on)` request the editor drains + applies.
    physics_pause_request: Rc<RefCell<Option<bool>>>,
    /// Gameplay ticks requested by `physics.step([n])` — the scriptable frame-stepper.
    frame_step_request: Rc<std::cell::Cell<u32>>,
    /// Mirror of the editor's physics-paused state (`physics.isPaused()`).
    physics_paused: Rc<std::cell::Cell<bool>>,
    /// A pending mouse-lock request from `input.lockMouse()` / `input.unlockMouse()`:
    /// `Some(true)` = lock (grab + hide the cursor), `Some(false)` = unlock, `None` = no
    /// change this frame. The editor drains it after `run` and applies it to the window.
    mouse_lock: Rc<RefCell<Option<bool>>>,
    /// Keys the HOST answers itself, so a script polling one is never going to
    /// see it — `(script name, why)`, filled by the driver
    /// ([`ScriptHost::set_reserved_keys`]). The editor reserves Play/Pause/Step;
    /// a headless harness reserves nothing.
    ///
    /// It exists so the first poll of such a key writes a Console line instead of
    /// returning `false` forever (`floptle/0084`). Being unavailable used to look
    /// exactly like not being pressed, which is why a game shipped an inventory
    /// bound to Tab and heard about it from a player rather than from a test.
    reserved_keys: ReservedKeys,
    /// Where this frame's time went, per subsystem and per script
    /// (`floptle/0077`). Written by the driver and by [`ScriptHost::run_pass`],
    /// read by the editor readout and by the Lua `perf` table — one structure, so
    /// a game's own budget assertion and the number on screen cannot disagree.
    ///
    /// Off by default and free while off. It exists because "the engine is slow"
    /// was the only report a game could make, and four such reports turned out to
    /// be four different numbers the game could have read itself.
    profile: SharedProfile,
    /// The player's accessibility settings, shared with `access.*` in Lua and
    /// read by the driver each frame (`floptle/0079`).
    access: crate::access_api::SharedAccess,
    /// Captions `caption(...)` asked for, drained by the driver and drawn by the
    /// engine so every game gets the same readable placement.
    caption_queue: crate::access_api::CaptionQueue,
    /// `params.X = value` writes queued this pass — (entity, script kind, key,
    /// value). Flushed to the node's stored `ScriptInst` params so tunables are
    /// TWO-WAY: the write persists across frames and shows live in the
    /// Inspector (and reverts on Stop like every play-mode change). Numbers
    /// AND strings; only DECLARED tunables persist (a key in `defaults` or the
    /// stored params).
    param_writes: RefCell<Vec<(u32, String, String, ParamWrite)>>,
    /// Pending `scene.load(...)` / `scene.unload(...)` requests. The driver
    /// drains them and performs each between frames — locally when
    /// offline/hosting, over the wire to every client in a session.
    scene_request: SceneQueue,
    /// `scene.onLoaded(fn)` subscriptions, as `(owner entity, callback)`. The
    /// owner is recorded so a subscription dies with the script that made it —
    /// otherwise a swap would leave every old scene's loading screen listening.
    /// A PERSISTENT node's subscription survives, which is the entire point:
    /// something has to outlive the load to be told about it.
    scene_loaded: Rc<RefCell<Vec<(u32, mlua::RegistryKey)>>>,
    /// Every WaterVolume in the scene, refreshed by the driver before scripts
    /// run — what `water.depthAt` / `water.at` read.
    water_volumes: Rc<RefCell<Vec<water_api::WaterInfo>>>,
    /// `water.setFrozen(node, on)` requests, drained by the driver.
    water_freeze: Rc<RefCell<Vec<(u32, bool)>>>,
    /// Scatter sources scripts declared (`floptle/0036`) — resolved into
    /// drawable instances by the driver, never into scene nodes.
    scatter_sources: scatter_api::Sources,
    /// The running scene's name, fed by the driver — what `scene.current()` reads.
    scene_name: Rc<RefCell<String>>,
    /// The focused UI element, fed by the engine each frame: what
    /// `ui.focused()` and `node.focused` read. Not a component — a focus ring
    /// that survived into a saved scene would be a bug.
    ui_focus: Rc<RefCell<Option<u32>>>,
    /// A pending `ui.focus(...)` (last call this frame wins), drained by the
    /// engine after the run.
    ui_focus_request: Rc<RefCell<Option<Option<u32>>>>,
    /// The drag in flight as `(source, target under it)` — `ui.dragging()` and
    /// `ui.dropTarget()`. Also set for the one frame the `dropped` hooks run.
    ui_drag: UiDragCell,
    /// Animator state per entity (layers/states/time), fed by the editor before `run`
    /// so scripts can read `anim:state()`, `anim:time()`, `anim:clips()`, ….
    anim_info: Rc<RefCell<HashMap<u32, AnimInfo>>>,
    /// Animator commands scripts queued this frame (`anim:play(...)` etc.), drained by
    /// the editor and applied to the controller runtimes before they advance — so intent
    /// set this frame lands this frame.
    anim_commands: Rc<RefCell<Vec<(u32, AnimCmd)>>>,
    /// Particle-system state per entity (playing/alive/asset), fed by the editor
    /// before `run` so scripts can read `node:particles():isPlaying()` / `:alive()`.
    vfx_info: Rc<RefCell<HashMap<u32, VfxInfo>>>,
    /// Particle commands scripts queued this frame (`node:particles():play()` etc.),
    /// drained by the editor and applied before the effects advance.
    vfx_commands: Rc<RefCell<Vec<(u32, VfxCmd)>>>,
    /// Audio commands scripts queued this frame (`audio.play(...)`, sound and
    /// mixer-track handles), drained by the editor and applied to the engine.
    audio_commands: Rc<RefCell<Vec<AudioCmd>>>,
    /// Audio playback mirror (script sounds + node AudioSources), fed by the
    /// editor before `run` so `sound:isPlaying()` / `:position()` read live state.
    audio_info: Rc<RefCell<AudioInfo>>,
    /// Debug-draw commands scripts queued this frame (`gizmo.line(...)` etc.) —
    /// immediate mode: drained by the editor each frame and drawn for one frame.
    gizmos: Rc<RefCell<Vec<GizmoCmd>>>,
    /// Fire-and-forget one-shot effects scripts requested this frame via
    /// `spawnEffect(key, x, y, z)`. The editor spawns a detached instance at each
    /// point; it plays once and auto-despawns.
    spawn_effects: Rc<RefCell<Vec<SpawnedEffect>>>,
    /// Prefab instances scripts requested this frame via `spawn(prefab, …)` —
    /// drained by the driver, which spawns the subtree + wires physics, then
    /// invokes each request's callback with the new root's handle.
    spawn_requests: Rc<RefCell<Vec<SpawnRequest>>>,
    /// This tick's `draw.line(...)` segments (immediate mode; drained per tick).
    draw_lines: Rc<RefCell<Vec<DrawLine>>>,
    /// This tick's `draw.tri/cone/disc(...)` filled triangles (immediate mode).
    draw_tris: Rc<RefCell<Vec<DrawTri>>>,
    draw_rects: Rc<RefCell<Vec<DrawRect>>>,
    draw_texts: Rc<RefCell<Vec<DrawText>>>,
    /// The `http.*` bridge: callbacks waiting on a reply, the caps, and the
    /// session generation that keeps a stale reply out of a fresh Play.
    http: Rc<RefCell<http_api::HttpState>>,
    /// The `account.*` bridge: the player's Foverse account and the Cloud calls
    /// waiting on a reply. Built lazily inside — a project that never signs
    /// anybody in never touches the OS keyring.
    account: Rc<RefCell<account_api::AccountState>>,
    /// True while the tick pass is running — `http.*` warns once when called
    /// from there, because nothing about a reply's timing can be replayed.
    http_in_fixed: Rc<std::cell::Cell<bool>>,
    /// Per-assembly mirror (`assembly.info`), fed by the driver each frame.
    assembly_info: Rc<RefCell<HashMap<u32, assembly_api::AssemblyInfo>>>,
    /// Per-part contact loads for the last tick (`assembly.impacts`), fed by
    /// the driver each tick — the damage/stress raw material.
    assembly_impacts: Rc<RefCell<HashMap<u32, Vec<assembly_api::AssemblyImpact>>>>,
    /// Queued `assembly.*` commands (held forces, impulses, splits), drained
    /// by the driver after the script pass.
    assembly_cmds: Rc<RefCell<Vec<assembly_api::AssemblyCmd>>>,
    /// Nodes scripts asked to remove via `destroy(node)` / `node:destroy()`
    /// (entity indices) — drained by the driver, which despawns the subtree
    /// and its physics bodies.
    destroy_queue: Rc<RefCell<Vec<u32>>>,
    /// The `net.*` bridge: queued session commands, mirrored session state,
    /// `net.on` handlers, and the current-instance marker (docs/netcode-design.md §8).
    net: net_api::SharedNet,
    /// Per-(entity, script) `synced` STORE tables (the raw values behind the
    /// proxy scripts see) — the host collects them for the server session and
    /// writes received updates into them on clients. Shared (Rc) with the
    /// `net.rewind` closure, which swaps historical values in around a
    /// lag-compensated handler and restores after.
    synced_stores: Rc<RefCell<HashMap<(u32, String), Table>>>,
    /// (eid, script, var) combos already warned about failing the replication
    /// guardrails — so a hot loop doesn't spam the Console every tick.
    synced_warned: std::collections::HashSet<(u32, String, String)>,
    /// `(script kind, param name)` already reported as stored-but-unread this
    /// session, so a param carried on eighteen instances of the same script is
    /// ONE Console line rather than eighteen (`floptle/0068`).
    param_warned: std::collections::HashSet<(String, String)>,
    /// `(script kind, key)` combos already reported as shadowing a `findScript`
    /// handle's own key (`floptle/0085`) — one line per script per session, not
    /// one per instance.
    handle_key_warned: std::collections::HashSet<(String, String)>,
    /// Entities whose scripts are SKIPPED this session (a networked CLIENT
    /// doesn't run server-authoritative nodes' scripts — their state arrives
    /// in snapshots; docs/netcode-design.md §6). Set by the driver.
    script_skip: std::collections::HashSet<u32>,
    /// Entities skipped in the PER-FRAME pass only: a predicted node's
    /// `update` re-runs on the gameplay tick (`run_frame_for`) so client and
    /// server integrate identically.
    frame_skip: std::collections::HashSet<u32>,
    /// Entities whose TICKS a driver owns (the rollback driver): their
    /// `fixedUpdate` and `update` run from there, so the global passes skip
    /// them — but their `lateUpdate` does NOT run there and is NOT skipped
    /// here. Separate from `script_skip` because a driver-owned node is still
    /// locally simulated; only the scheduling moved. floptle/0042.
    driver_skip: std::collections::HashSet<u32>,
    /// Set while the rollback driver is RE-SIMULATING ticks it already ran
    /// (`docs/rollback-netcode-design.md` §4). Scripts read it as
    /// `net.replaying()`; the engine uses it to discard the one-shot side
    /// effects a replay re-fires. Shared with the Lua closures.
    replaying: Rc<std::cell::Cell<bool>>,
    /// Queue lengths captured by [`ScriptHost::begin_replay`] so
    /// [`ScriptHost::end_replay`] drops exactly what the replay added — and
    /// nothing the live tick before it queued.
    replay_marks: Option<ReplayMarks>,
}

/// Where each suppressed one-shot queue stood when a replay began (§4).
///
/// Gating at the DRAIN rather than inside each Lua closure is deliberate: a
/// closure-side check has to be remembered at every new call site, and the one
/// that gets forgotten is a doubled hit spark nobody traces back to netcode.
/// Truncation catches every producer of a gated queue by construction.
#[derive(Clone, Copy, Debug)]
struct ReplayMarks {
    spawn_effects: usize,
    audio_commands: usize,
    spawn_requests: usize,
    destroy_queue: usize,
    net_cmds: usize,
    logs: usize,
}

/// One immediate-mode debug-draw command from a script's `gizmo.*` call.
/// World-space; lives for exactly one frame.
#[derive(Clone, Copy, Debug)]
pub enum GizmoCmd {
    Line { a: [f32; 3], b: [f32; 3], color: [f32; 3] },
    Sphere { center: [f32; 3], radius: f32, color: [f32; 3] },
    Point { pos: [f32; 3], size: f32, color: [f32; 3] },
}

/// A `gizmo.*` call's optional trailing color (0–1 floats), else the default green.
fn gizmo_color(r: Option<f64>, g: Option<f64>, b: Option<f64>) -> [f32; 3] {
    match (r, g, b) {
        (Some(r), Some(g), Some(b)) => [r as f32, g as f32, b as f32],
        _ => [0.35, 1.0, 0.45],
    }
}

/// The animator state of one entity, mirrored to scripts each frame.
#[derive(Clone, Debug, Default)]
pub struct AnimInfo {
    /// Per layer, base first: (layer name, current state, time seconds, finished).
    pub layers: Vec<(String, Option<String>, f32, bool)>,
    /// Every playable state across all layers, with its clip's authored duration and
    /// events. Behind an `Rc` because the mirror is rebuilt every frame while this half
    /// changes only when a controller rebinds — the driver caches it and clones the
    /// pointer.
    pub clips: Rc<Vec<ClipInfo>>,
}

/// One playable state's clip, as authored. Read-only — the game bakes integer frame data
/// out of this at load (`anim:events` / `anim:duration`), rather than letting float
/// playback events drive gameplay, which no rollback replay could reproduce.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipInfo {
    pub name: String,
    /// Authored clip length in seconds.
    pub duration: f32,
    /// `(t seconds, function name)`, ascending by `t`.
    pub events: Vec<(f32, String)>,
}

/// One queued `node:animator()` command.
#[derive(Clone, Debug)]
pub enum AnimCmd {
    /// Transition to a state. `fade` overrides the controller's fade table;
    /// `restart` re-enters even if the state is already playing.
    Play { state: String, layer: Option<String>, fade: Option<f32>, restart: bool },
    /// Stop a layer (`None` = every layer) — fades out / falls back to default.
    Stop { layer: Option<String>, fade: Option<f32> },
    /// Global playback speed multiplier.
    SetSpeed(f32),
    SetLayerWeight { layer: String, weight: f32 },
    /// Scrub the current state of `layer` (`None` = base) to `t` seconds.
    Seek { t: f32, layer: Option<String> },
}

/// The particle-system state of one node, mirrored to scripts each frame so
/// `node:particles():isPlaying()` / `:alive()` read live values.
#[derive(Clone, Debug, Default)]
pub struct VfxInfo {
    /// A live effect instance is emitting/ageing on this node right now.
    pub playing: bool,
    /// Live particle count across the effect's tracks.
    pub alive: u32,
    /// The effect asset key the node's `ParticleSystem` references.
    pub asset: String,
}

/// A one-shot effect a script requested via `spawnEffect(...)`: (asset key, world
/// position). The editor spawns a detached instance for each.
/// `(effect key, world point, emitter world velocity)`. The velocity (default 0) lets
/// inherit-velocity tracks ride the emitter's momentum — see `spawnEffect`.
pub type SpawnedEffect = (String, [f64; 3], [f64; 3]);

/// One queued `node:particles()` command, drained by the editor and applied to the
/// live VFX instances before they advance (so intent set this frame lands this frame).
#[derive(Clone, Debug)]
pub enum VfxCmd {
    /// Start the node's effect if it isn't already playing (spawns an instance).
    Play,
    /// Stop + despawn the node's effect (its live particles vanish).
    Stop,
    /// Restart from t = 0 (re-spawns a fresh instance) — re-fire a one-shot burst.
    Restart,
    /// Live emission scale (0..~2): multiplies rates/burst counts and shades
    /// particle size — `ps:setIntensity(throttle)` drives an engine plume.
    Intensity(f32),
    /// Aim every Beam track's endpoint at a WORLD-space point — the editor
    /// converts it to effect-local before applying (`ps:setBeamEnd(x, y, z)`).
    SetBeamEnd([f64; 3]),
}

/// Where a script-spawned sound sits: nowhere (flat), a fixed world point, or
/// following a node (entity index).
#[derive(Clone, Copy, Debug)]
pub enum AudioAt {
    Flat,
    Pos([f64; 3]),
    Node(u32),
}

/// One queued `audio` command, drained by the editor after `run` and applied
/// to the audio engine (`handle` = script-side sound id; `ent` = entity index
/// of a node's AudioSource).
#[derive(Clone, Debug)]
pub enum AudioCmd {
    Play { handle: u32, clip: String, at: AudioAt, params: Box<floptle_audio::PlayParams> },
    Stop { handle: u32 },
    Pause { handle: u32, paused: bool },
    /// Set a numeric knob on a playing sound ("volume" | "pitch" | "pan").
    SetParam { handle: u32, field: String, value: f64 },
    SetTrack { handle: u32, track: String },
    Move { handle: u32, pos: [f64; 3] },
    Seek { handle: u32, secs: f64 },
    StopAll,
    SourcePlay { ent: u32 },
    SourceStop { ent: u32 },
    SourcePause { ent: u32, paused: bool },
    SourceSetClip { ent: u32, clip: String },
    SourceSeek { ent: u32, secs: f64 },
    TrackVolume { track: String, db: f64 },
    TrackPan { track: String, pan: f64 },
    TrackMuted { track: String, muted: bool },
    TrackSoloed { track: String, soloed: bool },
}

/// Live playback state of one sound / source, mirrored for script reads.
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioPlayState {
    pub playing: bool,
    pub paused: bool,
    /// Playhead in seconds.
    pub position: f64,
}

/// The audio mirror the editor feeds before each `run`: script one-shots by
/// handle, node AudioSources by entity index.
#[derive(Clone, Debug, Default)]
pub struct AudioInfo {
    pub sounds: HashMap<u32, AudioPlayState>,
    pub sources: HashMap<u32, AudioPlayState>,
}

/// A mirror of the scene graph the Lua node/script handles read and write, synced from
/// the ECS at the start of each `run` and flushed back at the end. It decouples the Lua
/// handles (which can persist across frames, e.g. a cached manager reference) from the
/// `&mut World` borrow, and lets one script reach any other node by hierarchy or name.
/// The queue the construction API pushes into, drained each pass by
/// `flush_writes`.
pub(crate) type RichSetQueue = Rc<RefCell<Vec<(u32, RichSet)>>>;

#[derive(Default)]
pub(crate) struct SceneMirror {
    /// Stable iteration order (entity index), for deterministic name lookups.
    order: Vec<u32>,
    names: HashMap<u32, String>,
    /// name → FIRST entity in scene order with that name: the O(1) index behind
    /// `find()` and node-reference params (no more linear scans per call).
    by_name: HashMap<String, u32>,
    parent: HashMap<u32, u32>,
    children: HashMap<u32, Vec<u32>>,
    /// Entity → the script kinds attached to it (for `node:getscript`).
    scripts: HashMap<u32, Vec<String>>,
    /// script kind → every entity carrying it, IN SCENE ORDER — the index behind
    /// `findScript` / `findScripts` (`floptle/0063`).
    ///
    /// These are the calls a gameplay codebase makes most, because they are how
    /// one script reaches another and the alternative (an Inspector wire) does
    /// not exist for a singleton sixteen panels want, for "is any craft being
    /// flown", or for anything spawned at runtime. Walking the scene and
    /// string-comparing per node made the cost of asking scale with the scene:
    /// one real project issued 126 full-scene scans a frame, none of them
    /// carelessly written.
    ///
    /// Scene order is load-bearing — `findScript` returns the FIRST, and call
    /// sites depend on which — so this is built in the same pass and the same
    /// order as `order` and `by_name`.
    by_kind: HashMap<String, Vec<u32>>,
    /// tag → every entity carrying it, in scene order. Same reasoning as
    /// `by_kind`, for `findTagged`.
    by_tag: HashMap<String, Vec<u32>>,
    /// Live transforms (read/written by node handles; flushed to the ECS after `run`).
    transforms: HashMap<u32, Transform>,
    /// Mesh nodes' current model path (so a script can read `node.model`).
    models: HashMap<u32, String>,
    /// Tilemap nodes' grid and what it is cut from, so a handle can answer
    /// `tm:get` / `tm:size` / `tm:solid` without reaching into the world
    /// (`floptle/0058`).
    tilemaps: HashMap<u32, TilemapMirror>,
    /// The project's loaded tilesets, keyed by their project-relative path.
    ///
    /// LENT by the host (`ScriptHost::set_tilesets`), the same way the layer table
    /// is: the script host does no file I/O of its own, so who owns the parse is
    /// unambiguous and a headless test can hand in a tileset without a project on
    /// disk. A path with no entry means the tileset failed to load or was never
    /// referenced — `tm:solid` then answers `false` rather than guessing, and the
    /// editor is the one that says so in the Console.
    tilesets: HashMap<String, floptle_tiles::TileSet>,
    /// Entities that ARE sprite batches, so `node:sprites()` can refuse a node
    /// that is not one instead of handing back a handle whose every draw is
    /// silently dropped.
    sprite_batches: std::collections::HashSet<u32>,
    /// UI elements' current text (so a script can read `node.text`).
    ui_texts: HashMap<u32, String>,
    /// UI elements' current style name (so a script can read `node.style`).
    ui_styles: HashMap<u32, String>,
    /// UI images' current texture path (so a script can READ `node.texture`,
    /// not just write it — the asymmetry was half of floptle/0052).
    ui_textures: HashMap<u32, String>,
    /// Nodes that carry an explicit `Visible` component (so a script can read
    /// `node.visible`; absent = visible by default).
    visible: HashMap<u32, bool>,
    /// Nodes carrying `floptle_core::Disabled` THEMSELVES (not inherited) — what
    /// `node.enabled` reads back. Inheritance is resolved by the engine, not mirrored.
    disabled: std::collections::HashSet<u32>,
    /// Nodes carrying `floptle_core::Persistent` THEMSELVES — what
    /// `node.persistent` reads back. Same rule as `disabled`: the subtree
    /// inheritance is the engine's to resolve, not the mirror's to duplicate.
    persistent: std::collections::HashSet<u32>,
    /// Nodes with an explicit `Layer` component, by layer NAME (absent =
    /// "Default"). Read by `node.layer`.
    layers: HashMap<u32, String>,
    /// Nodes' tag lists (absent = untagged). Read by `node.tags` /
    /// `node:hasTag`, scanned by `findTagged`.
    tags: HashMap<u32, Vec<String>>,
    /// entity → component name → (field → value): the numeric fields scripts can read via
    /// `node:getcomponent("PointLight"/"RigidBody")`. Synced each run for read-back; writes
    /// go through `Shared::component_changes` and are flushed to the ECS after `run`.
    components: HashMap<u32, HashMap<String, HashMap<String, f64>>>,
    /// Repeater rows' 0-based index, read as `node.index`. Absent on
    /// everything a repeater didn't spawn.
    repeat_index: HashMap<u32, u32>,
    /// The colour-valued half of the same mirror (`e.fill`, `e.textColor`, …).
    component_colors: HashMap<u32, HashMap<String, HashMap<String, [f32; 4]>>>,
    /// Entity index → its `Entity` (with generation), so handle-written transforms flush
    /// back to the right ECS entity.
    ents: HashMap<u32, Entity>,
    /// Entities whose transform a handle wrote this frame (so we only flush those back —
    /// the current node still flushes via the value-table path).
    dirty: std::collections::HashSet<u32>,
}

/// A prefab instance a script requested via `spawn(prefab [, pos [, fn]])`:
/// the prefab name/path, an optional world position for its first root, and
/// an optional callback (a Lua registry key) the driver invokes with the new
/// root's node handle once it exists (`ScriptHost::call_spawn_callback`).
pub struct SpawnRequest {
    pub prefab: String,
    pub pos: Option<[f64; 3]>,
    pub cb: Option<mlua::RegistryKey>,
    /// Spawn the prefab's root(s) as CHILDREN of this entity (kept at the
    /// world `pos` — the driver converts to the parent's local frame). How a
    /// vessel prefab's parts land under an assembly root.
    pub parent: Option<u32>,
}

/// A `createNode(name [, parent] [, fn])` request: a plain node (Empty matter)
/// the editor's spawn drain creates; `cb` then receives the new node's handle
/// — the construction hook for script-built content (editor actions, procgen).
pub struct CreateRequest {
    pub name: String,
    pub parent: Option<u32>,
    pub cb: Option<mlua::RegistryKey>,
}

/// One value in a rich component write (`node:setCelestial{...}` and friends):
/// numbers, strings and 3-vectors all flow (the numeric `component_changes`
/// mirror can't carry strings/colors).
#[derive(Clone, Debug)]
pub enum CompVal {
    Num(f64),
    Str(String),
    Vec3([f64; 3]),
}

/// A queued construction-API write, applied in the host's flush: whole
/// component field-sets (the component is inserted with defaults if the node
/// lacks it) and Matter swaps.
#[derive(Debug)]
pub enum RichSet {
    Celestial(Vec<(String, CompVal)>),
    Material(Vec<(String, CompVal)>),
    MatterTerrain(u32),
    /// `node:setPrimitive(shape [, color])`. The shape is already PARSED — the
    /// name was checked at the call, where a misspelling can still name a line
    /// (`floptle/0082`).
    MatterPrimitive(floptle_core::Shape, [f64; 3]),
    /// `node:setTilemap{...}` — build (or re-shape) a 2D grid on this node.
    MatterTilemap {
        cols: u32,
        rows: u32,
        tile: f32,
        data: Vec<u32>,
        /// `None` KEEPS whatever the node already referenced — `setTilemap` is
        /// also how a script resizes a map, and dropping the tileset on a resize
        /// would silently un-solid the level.
        tileset: Option<String>,
    },
    /// `node:setSpriteBatch{ size = }` — the other half of the 2D pair. A
    /// game's sprite styles are DATA (one batch per material, one material per
    /// style), so the nodes that draw them have to be makeable from the same
    /// Lua that declares them, not authored one-by-one into a scene and kept in
    /// sync by nothing.
    MatterSpriteBatch { size: f32 },
    /// `tm:set(x, y, cell)` writes, batched per call site. Applied in order, so
    /// two writes to one square land the way the script wrote them.
    TileCells(Vec<(u32, u32, u32)>),
    /// `tm:resize{...}` — a new grid size, keeping whatever overlaps. `ox`/`oy`
    /// are where the old top-left lands in the new grid, so growing a map
    /// upward is `oy = 1` rather than a second call shape.
    TileResize { cols: Option<u32>, rows: Option<u32>, ox: i32, oy: i32 },
    /// `tm:autotile(x0, y0, x1, y1)` — recompute the region's autotiled squares
    /// (and the one-square ring around it, which is where the stale edges are).
    TileAutotile { x0: i32, y0: i32, x1: i32, y1: i32 },
    /// On-demand generation spec (RON `PlanetFill`) for a Terrain node —
    /// `None` clears it. See `floptle_core::TerrainGen` (G2 galaxy streaming).
    TerrainGen(Option<String>),
    /// `node:setCamera{...}` — aim a camera, hand it authority, and point it at
    /// a live `rt:<name>` texture at a chosen size and refresh rate
    /// (`floptle/0078`).
    ///
    /// Every field is an `Option` of a value the engine will act on, not a
    /// `(name, value)` pair: the table is validated at the CALL, where a
    /// traceback points at the line that wrote it, so nothing here can be
    /// silently unread on the way out.
    MatterCamera {
        fov_y: Option<f32>,
        active: Option<bool>,
        target: Option<String>,
        target_w: Option<u32>,
        target_h: Option<u32>,
        target_hz: Option<f32>,
        cull_mask: Option<u32>,
        /// `projection = "orthographic" | "perspective"`, parsed at the call.
        ortho: Option<bool>,
        ortho_height: Option<f32>,
    },
}

/// What a script can ask about a tilemap node without reaching into the ECS.
///
/// The grid is cloned per frame, which is the same deal every other mirror entry
/// makes: a handle's reads have to be answerable inside a Lua closure that holds
/// no `&World`. A 200x200 map is 40,000 `u32` — 160 KB a frame — so a scene of
/// several large tilemaps is worth knowing about, and the alternative (handing
/// Lua a live borrow) is not one this host can offer.
#[derive(Clone, Debug, Default)]
pub(crate) struct TilemapMirror {
    pub(crate) cols: u32,
    pub(crate) rows: u32,
    /// World edge length of one square — what `tm:tileSize()` answers and what
    /// the world/cell conversions divide by.
    pub(crate) tile: f32,
    /// Row-major packed squares (cell index + orientation).
    pub(crate) data: Vec<u32>,
    /// Project-relative `.tileset.ron`, or empty.
    pub(crate) tileset: String,
}

/// The interior-mutable state the Lua handle closures share with the host: the scene
/// mirror, the physics body bridges, and the per-(entity, script) environments.
#[derive(Clone)]
struct Shared {
    scene: Rc<RefCell<SceneMirror>>,
    bodies: Rc<RefCell<HashMap<u32, BodyState>>>,
    ui_rects: Rc<RefCell<HashMap<u32, [f32; 4]>>>,
    body_changes: Rc<RefCell<HashMap<u32, [f32; 3]>>>,
    body_height_changes: Rc<RefCell<HashMap<u32, f32>>>,
    /// Cross-node POSITION writes onto entities that HAVE a physics body —
    /// the driver TELEPORTS the body there (otherwise the physics writeback
    /// stomps the transform next frame and the write silently vanishes).
    body_pos_changes: Rc<RefCell<HashMap<u32, [f64; 3]>>>,
    /// This frame's `b:draw(...)` calls per sprite-batch entity.
    ///
    /// IMMEDIATE MODE, like `draw.*` and `gizmo.*`: the list is taken every
    /// pass and becomes that node's whole set of sprites, so what you drew this
    /// frame is exactly what shows and there is no `clear()` anyone can forget.
    /// A retained list would leak for as long as the game ran.
    sprite_draws: Rc<RefCell<HashMap<u32, Vec<floptle_core::Sprite>>>>,
    /// `node:setShaderParam(...)` writes, drained by the editor per frame.
    shader_param_sets: ShaderParamSets,
    /// (entity index, script kind) → that instance's live Lua environment, so a
    /// script handle can read its state, call its methods, and read its params.
    ///
    /// A `RegistryKey`, resolved to a `Table` at each use. It held the `Table`
    /// directly until a Table alive in Rust turned out to cost a slot on mlua's
    /// AUXILIARY ref stack, which is bounded near 8,000 — so a scene of a few
    /// thousand scripted nodes exhausted it and the engine PANICKED, in the
    /// editor, where unsaved work lives (`floptle/0069`). The registry is an
    /// ordinary Lua table with no such bound, and the key drops itself.
    envs: Rc<RefCell<HashMap<(u32, String), RegistryKey>>>,
    /// `node.model = ...` writes (entity index → asset path), applied to `Matter::Mesh`.
    model_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// `node.material = ...` writes (entity index → preset name / asset path).
    material_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// `node.visible = ...` writes (entity index → shown), applied as a `Visible` component.
    visible_changes: Rc<RefCell<HashMap<u32, bool>>>,
    /// `node.enabled = …` — switches the node (and its subtree) off/on. Separate from
    /// `visible`: that one only stops the draw, this also stops physics and scripts.
    enabled_changes: Rc<RefCell<HashMap<u32, bool>>>,
    /// `node.persistent = …` — whether the node (and its subtree) survives a
    /// scene swap. Applied as a `Persistent` marker; absence means "ordinary".
    persistent_changes: Rc<RefCell<HashMap<u32, bool>>>,
    /// `node.layer = "Name"` writes (entity index → layer name, pre-validated
    /// against the project's layer table), applied as a `Layer` component.
    layer_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// Tag edits (`node:addTag/removeTag`, `node.tags = {...}`): entity index →
    /// the node's FULL new tag list, applied as a `Tags` component.
    tag_changes: Rc<RefCell<HashMap<u32, Vec<String>>>>,
    /// The project's resolved layer table (names + collision matrix), lent by
    /// the driver at Play start — validates `node.layer` writes and resolves
    /// `raycast`'s named-layer filters to masks.
    layer_table: Rc<RefCell<floptle_core::Layers>>,
    /// `node.text = ...` writes (entity index → text), applied to the node's UI ElementSpec.
    ui_text_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// `node.style = ...` writes (entity index → style name), applied to the
    /// node's UI ElementSpec. A separate channel from the text one because they
    /// are different fields that happen to share a "string, read-your-writes"
    /// shape; one map would have to tag every entry with which field it meant.
    ui_style_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// The focused UI element, fed by the engine each frame — what
    /// `node.focused` reads.
    ui_focus: Rc<RefCell<Option<u32>>>,
    /// `node:getcomponent(name).field = value` writes: (entity, component, field) → number,
    /// flushed to the ECS after `run` (and read back the same frame).
    component_changes: ComponentWrites,
    component_colors: ComponentColorWrites,
    component_strs: ComponentStrWrites,
    /// Construction-API writes (`setCelestial`/`setMaterial`/`setTerrain`/
    /// `setPrimitive`), applied in the flush.
    rich_sets: Rc<RefCell<Vec<(u32, RichSet)>>>,
    /// Animator mirror (entity → layers/states), fed by the editor each frame.
    anim_info: Rc<RefCell<HashMap<u32, AnimInfo>>>,
    /// Animator commands queued by `node:animator()` handles this frame.
    anim_commands: Rc<RefCell<Vec<(u32, AnimCmd)>>>,
    /// Particle-system mirror (entity → playing/alive/asset), fed by the editor.
    vfx_info: Rc<RefCell<HashMap<u32, VfxInfo>>>,
    /// Particle commands queued by `node:particles()` handles this frame.
    vfx_commands: Rc<RefCell<Vec<(u32, VfxCmd)>>>,
    /// `destroy(node)` / `node:destroy()` requests (entity indices).
    destroy_queue: Rc<RefCell<Vec<u32>>>,
}

/// One queued two-way `params.X = ...` write: a number or a string.
#[derive(Clone, Debug)]
pub(crate) enum ParamWrite {
    Num(f32),
    Str(String),
}

/// A script's declared defaults surface: numeric params + reference params +
/// string params (plain non-sentinel string defaults).
pub type ScriptDefaults = (Vec<(String, f32)>, Vec<(String, RefKind)>, Vec<(String, String)>);

/// What a script's reference param (declared in `defaults`) binds to — drives
/// the Inspector's picker (candidate filtering) and the runtime handle type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefKind {
    /// `noderef()` — a node handle.
    Node,
    /// `scriptref("health")` — a script handle for that script on the wired node.
    Script(String),
    /// `componentref("RigidBody")` — a component handle on the wired node.
    Component(String),
}

/// A physics body's state exposed to its node's scripts.
#[derive(Clone, Copy, Debug)]
pub struct BodyState {
    pub vel: [f32; 3],
    /// The body's "up" (−gravity) — Y for normal gravity, radial on a planet. Lets a
    /// controller script move along the surface and jump correctly on any world.
    pub up: [f32; 3],
    pub grounded: bool,
    /// Current capsule standing height — a controller reads it and writes `node.height`
    /// to crouch (the engine resizes the capsule, feet planted).
    pub height: f32,
    /// The BODY's world position at the start of this tick — what
    /// `node.tickX/tickY/tickZ/tickPos` read, and what a write to them sets.
    ///
    /// Not the same thing as `node.x`. Between ticks the node's transform holds
    /// the *interpolated render pose* (lerped by the frame's alpha), so reading
    /// it inside `fixedUpdate` is a frame-rate-dependent read that no replay can
    /// reproduce — and writing `node.x = node.x + d` there teleports the body
    /// onto the visual position, which is the classic "the visuals take the
    /// knockback but the hitbox stays put" bug
    /// (`docs/rollback-netcode-design.md` §3).
    pub pos: [f64; 3],
    /// The floor under the body (`node.groundNormal`) — `Some` exactly when
    /// `grounded`. Align a character to the slope, judge how steep it is, or
    /// decide a landing is too hard.
    pub ground_normal: Option<[f32; 3]>,
    /// The steepest surface the body is pressed against, when it is too steep
    /// to stand on (`node.wallNormal`).
    ///
    /// This is what stops a walking controller from launching itself: driving
    /// into a cliff means the solver pushes the capsule out along a normal with
    /// an upward component, every frame, which reads as being fired into the
    /// sky. A controller that can SEE the wall simply stops pushing into it.
    pub wall_normal: Option<[f32; 3]>,
}

impl Default for BodyState {
    fn default() -> Self {
        Self {
            vel: [0.0; 3],
            up: [0.0, 1.0, 0.0],
            grounded: false,
            height: 2.0,
            pos: [0.0; 3],
            ground_normal: None,
            wall_normal: None,
        }
    }
}

/// The Lua scripts shipped into every new project, for the compile check below.
/// Kept beside the host rather than in the editor so a syntax error is caught by
/// the crate that would actually have to run it.
#[cfg(test)]
const SHIPPED_SCRIPTS: &[(&str, &str)] = &[
    ("freelook.lua", include_str!("../../../assets/scripts/freelook.lua")),
    ("first_person.lua", include_str!("../../../assets/scripts/first_person.lua")),
    ("third_person.lua", include_str!("../../../assets/scripts/third_person.lua")),
    (
        "third_person_camera.lua",
        include_str!("../../../assets/scripts/third_person_camera.lua"),
    ),
    ("fighter.lua", include_str!("../../../assets/scripts/fighter.lua")),
    // A starting point for strategy games: an isometric camera you pan with
    // WASD or the screen edge, commandable units, and the mouse layer that
    // selects and orders them.
    ("rts_camera.lua", include_str!("../../../assets/scripts/rts_camera.lua")),
    ("rts_unit.lua", include_str!("../../../assets/scripts/rts_unit.lua")),
    ("rts_commander.lua", include_str!("../../../assets/scripts/rts_commander.lua")),
    ("sword.lua", include_str!("../../../assets/scripts/sword.lua")),
    ("rotate.lua", include_str!("../../../assets/scripts/rotate.lua")),
    ("pulsate.lua", include_str!("../../../assets/scripts/pulsate.lua")),
    ("float.lua", include_str!("../../../assets/scripts/float.lua")),
    ("hand.lua", include_str!("../../../assets/scripts/hand.lua")),
    ("portal.lua", include_str!("../../../assets/scripts/portal.lua")),
    ("parry_dummy.lua", include_str!("../../../assets/scripts/parry_dummy.lua")),
    ("player_spawner.lua", include_str!("../../../assets/scripts/player_spawner.lua")),
    ("fixedTest.lua", include_str!("../../../assets/scripts/fixedTest.lua")),
    ("ui_demo.lua", include_str!("../../../assets/scripts/ui_demo.lua")),
    ("ui_demo_button.lua", include_str!("../../../assets/scripts/ui_demo_button.lua")),
    ("ui_demo_field.lua", include_str!("../../../assets/scripts/ui_demo_field.lua")),
    ("ui_demo_row.lua", include_str!("../../../assets/scripts/ui_demo_row.lua")),
    ("ui_demo_slot.lua", include_str!("../../../assets/scripts/ui_demo_slot.lua")),
    ("web_login.lua", include_str!("../../../assets/scripts/web_login.lua")),
];

#[cfg(test)]
mod shipped_script_tests {
    use super::SHIPPED_SCRIPTS;

    /// Every shipped script must at least COMPILE.
    ///
    /// A script only reports a syntax error when something in a scene happens
    /// to run it, so a broken default could sit in a release unnoticed — and
    /// `freelook.lua` is attached to every new project's camera.
    #[test]
    fn shipped_scripts_compile() {
        let lua = mlua::Lua::new();
        for (name, body) in SHIPPED_SCRIPTS {
            if let Err(e) = lua.load(*body).set_name(*name).into_function() {
                panic!("{name} does not compile:\n{e}");
            }
        }
    }

    /// The solar demo's scripts must compile too.
    ///
    /// They are not shipped into new projects, so `SHIPPED_SCRIPTS` doesn't
    /// cover them — but they are the largest body of real Lua in the repo, they
    /// are what the demo project runs, and a syntax error in one only surfaces
    /// when someone opens the scene it is attached to. Read from disk rather
    /// than `include_str!` so adding a script to the demo needs no edit here.
    #[test]
    fn the_solar_demo_scripts_compile() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../solar/scripts");
        let Ok(rd) = std::fs::read_dir(&dir) else { return };
        let lua = mlua::Lua::new();
        let mut n = 0;
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("lua") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("readable");
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            n += 1;
            if let Err(e) = lua.load(&src).set_name(&name).into_function() {
                panic!("{name} does not compile:\n{e}");
            }
        }
        assert!(n > 10, "expected the solar demo's scripts, saw {n}");
    }

    /// …and every controller/camera example must RUN — `start` and a few frames
    /// of `update`/`lateUpdate` against a node with a physics body — without a
    /// single runtime error.
    ///
    /// Compiling is not the same as working: a script that calls a method on a
    /// nil `node.up`, passes a vec3 where a number is wanted, or spells an API
    /// name that no longer exists compiles perfectly and dies on frame one. The
    /// 0.20.0 rewrite of these five to the readability API is exactly the kind
    /// of change that needs a gate stronger than `into_function()`.
    #[test]
    fn shipped_controller_scripts_run_without_errors() {
        use crate::ScriptHost;
        use floptle_core::transform::Transform;
        use floptle_core::{Scripts, World};
        use std::collections::HashMap;
        use std::io::Write;

        // The ones that drive a node every frame; the rest are UI/demo pieces
        // with scene dependencies a bare world can't stand in for.
        const DRIVERS: &[&str] = &[
            "first_person",
            "third_person",
            "third_person_camera",
            "rts_camera",
            "rts_unit",
            "freelook",
            "float",
            "rotate",
            "pulsate",
        ];

        let dir = std::env::temp_dir().join(format!("floptle-smoke-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in SHIPPED_SCRIPTS {
            let mut f = std::fs::File::create(dir.join(name)).unwrap();
            f.write_all(body.as_bytes()).unwrap();
        }

        for kind in DRIVERS {
            let mut world = World::default();
            let e = world.spawn();
            world.insert(e, Transform::IDENTITY);
            world.insert(e, floptle_core::Name("Player".into()));
            world.insert(e, floptle_core::Matter::Empty);
            world.insert(e, floptle_core::RigidBody::default());
            world.insert(
                e,
                Scripts(vec![floptle_core::ScriptInst {
                    kind: (*kind).into(),
                    enabled: true,
                    params: vec![],
                    refs: Vec::new(),
                    strs: Vec::new(),
                }]),
            );
            let mut host = ScriptHost::new();
            // The body bridge the physics step would publish: standing on flat
            // ground, moving, with a real up. Without it `node.vel` is nil and
            // every controller is testing something other than itself.
            let mut bodies = HashMap::new();
            bodies.insert(
                e.index(),
                crate::BodyState {
                    vel: [0.5, 0.0, -1.0],
                    up: [0.0, 1.0, 0.0],
                    grounded: true,
                    height: 2.0,
                    pos: [0.0, 0.0, 0.0],
                    ground_normal: Some([0.0, 1.0, 0.0]),
                    wall_normal: None,
                },
            );
            host.set_bodies(bodies);
            for _ in 0..3 {
                host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
            }
            assert!(host.errors().is_empty(), "{kind}.lua: {:?}", host.errors());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use floptle_core::math::EulerRot;
    use floptle_core::{Matter, ParticleSystem, RigidBody, Visible};

    use super::*;
    
    use crate::preprocess::*;
    use floptle_core::transform::Transform;
    use floptle_core::{Scripts, World};
    use std::io::Write;

    /// The editor-action path end-to-end at the script layer: `call_action`
    /// runs EXACTLY the named function (never `start`), the construction API
    /// (`setCelestial`/`setMaterial`) lands on the world, and `createNode` +
    /// `terrain.generatePlanet` sit queued for the editor to drain.
    #[test]
    fn editor_action_runs_one_function_and_queues_construction() {
        let dir = std::env::temp_dir().join(format!("floptle-action-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_script(
            &dir,
            "gen",
            r#"
--@editorButton Generate roll
defaults = { size = 30 }
function start(node) node.x = 999 end -- must NOT fire on an action
function roll(node)
  node:setCelestial{ mu = 5000, parent = "Sun", atmoColor = {0.2, 0.4, 0.9} }
  node:setMaterial{ unlit = true, emissiveStrength = 2 }
  createNode("Child", node, function(c)
    c:setTerrain(3)
    c:setTerrainGen{ radius = params.size, caveDepth = 12, seed = 99 }
  end)
  terrain.generatePlanet(3, { radius = params.size, caveDepth = 0 })
end
"#,
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, floptle_core::Name("Gen".into()));
        world.insert(e, Matter::Empty);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "gen".into(),
                enabled: true,
                params: vec![("size".into(), 42.0)],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        let ran = host.call_action(&mut world, &dir, e.index(), "gen", "roll");
        assert!(ran, "action failed: {:?}", host.errors());
        // start() must not have fired: the transform is untouched.
        assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 0.0);
        let c = world.get::<floptle_core::CelestialBody>(e).expect("setCelestial inserted");
        assert_eq!(c.mu, 5000.0);
        assert_eq!(c.parent, "Sun");
        assert!((c.atmo_color[2] - 0.9).abs() < 1e-5);
        let m = world.get::<floptle_core::Material>(e).expect("setMaterial inserted");
        assert!(m.unlit);
        assert_eq!(m.emissive_strength, 2.0);
        let creates = host.take_create_requests();
        assert_eq!(creates.len(), 1);
        assert_eq!(creates[0].name, "Child");
        assert_eq!(creates[0].parent, Some(e.index()));
        // Mimic the editor's drain (apply_spawn_batch): spawn the node, then run
        // the callback — its construction writes must land IMMEDIATELY (the drain
        // is the last flush an editor action gets; a transform-only flush here
        // left generator planets as Matter::Empty and their generated terrain
        // fields orphaned — "generated field … but no node carries it").
        let mut creates = creates;
        let child = world.spawn();
        world.insert(child, Transform::IDENTITY);
        world.insert(child, floptle_core::Name(creates[0].name.clone()));
        world.insert(child, Matter::Empty);
        let cb = creates.remove(0).cb.expect("create carried its callback");
        host.call_create_callback(&mut world, cb, child.index());
        match world.get::<Matter>(child) {
            Some(Matter::Terrain { id }) => assert_eq!(*id, 3),
            other => panic!("createNode callback's setTerrain(3) did not land: {other:?}"),
        }
        // setTerrainGen: the genspec lands as a RON PlanetFill that parses back
        // (the G2 on-demand generation contract — the streamer regenerates the
        // body from exactly this string).
        let spec = world
            .get::<floptle_core::TerrainGen>(child)
            .expect("setTerrainGen inserted the genspec");
        let fill: floptle_field::procgen::PlanetFill =
            ron::from_str(&spec.0).expect("genspec parses back to a PlanetFill");
        assert_eq!(fill.radius, 42.0); // Inspector-tuned param reached the spec
        assert_eq!(fill.cave_depth, 12.0);
        assert_eq!(fill.seed, 99);
        let gens = host.take_terrain_generates();
        assert_eq!(gens.len(), 1);
        assert_eq!(gens[0].0, 3);
        // Inspector-tuned params reach the action (42 overrides the default 30).
        assert_eq!(gens[0].1.radius, 42.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn write_script(dir: &Path, name: &str, body: &str) {
        let mut f = std::fs::File::create(dir.join(format!("{name}.lua"))).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn rotate_script_drives_yaw() {
        let dir = std::env::temp_dir().join("floptle_script_test_rotate");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "rotate",
            "defaults = { speed = 90 }\nfunction update(node, dt)\n  node.yaw = node.yaw + math.rad(params.speed) * dt\nend\n",
        );

        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Scripts(vec![floptle_core::ScriptInst {
            kind: "rotate".into(),
            enabled: true,
            params: vec![("speed".into(), 90.0)], refs: Vec::new(),
            strs: Vec::new(),
        }]));

        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0, 1.0); // 90 deg/s for 1s -> ~pi/2 yaw
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let tr = world.get::<Transform>(e).unwrap();
        let (yaw, _, _) = tr.rotation.to_euler(EulerRot::YXZ);
        assert!((yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-3, "yaw was {yaw}");
    }

    /// `params` is TWO-WAY: a script's `params.x = ...` write persists across
    /// frames (the next seed reads it back) and lands in the node's stored
    /// ScriptInst — the Inspector shows it live. Undeclared keys stay
    /// frame-local (they must not silently grow the Inspector).
    #[test]
    fn param_writes_persist_and_reach_the_stored_params() {
        let dir = std::env::temp_dir().join("floptle_script_test_param_write");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "zoom",
            "defaults = { d = 6 }\nfunction update(node, dt)\n  params.d = params.d - 1\n  params.ghost = 42\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "zoom".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.016, 0.0);
        host.run(&mut world, &dir, 0.016, 0.016);
        host.run(&mut world, &dir, 0.016, 0.032);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let scripts = world.get::<Scripts>(e).unwrap();
        let stored = &scripts.0[0].params;
        let d = stored.iter().find(|(k, _)| k == "d").map(|(_, v)| *v);
        assert_eq!(d, Some(3.0), "the write must persist and decrement each frame: {stored:?}");
        assert!(
            !stored.iter().any(|(k, _)| k == "ghost"),
            "undeclared keys stay frame-local: {stored:?}"
        );
    }

    /// STRING params: a `name = "text"` default seeds an Inspector-editable
    /// text tunable; stored overrides win over the default, script writes are
    /// two-way (persist + reach the stored strs), and undeclared string keys
    /// stay frame-local — the numeric rules, for text.
    #[test]
    fn string_params_seed_override_and_write_two_way() {
        let dir = std::env::temp_dir().join("floptle_script_test_str_params");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "portal",
            "defaults = { scene = \"hub\", label = \"door\" }\n\
             seen = \"\"\n\
             function update(node, dt)\n\
               seen = params.scene .. \"/\" .. params.label\n\
               params.label = \"door2\"\n\
               params.ghost = \"nope\"\n\
             end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "portal".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                // The Inspector override: THIS portal goes to the arena.
                strs: vec![("scene".into(), "arena".into())],
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.016, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        // The script read the override (not the default) + the default label.
        let env_seen: String = host
            .instance_env(e.index(), "portal")
            .and_then(|env| env.get::<String>("seen").ok())
            .unwrap_or_default();
        assert_eq!(env_seen, "arena/door");
        // The label write persisted to the stored strs; ghost did not.
        let scripts = world.get::<Scripts>(e).unwrap();
        let strs = &scripts.0[0].strs;
        assert_eq!(
            strs.iter().find(|(k, _)| k == "label").map(|(_, v)| v.as_str()),
            Some("door2"),
            "string writes are two-way: {strs:?}"
        );
        assert!(!strs.iter().any(|(k, _)| k == "ghost"), "undeclared stays frame-local");
        // Next frame seeds the persisted write back.
        host.run(&mut world, &dir, 0.016, 0.016);
        let env_seen: String = host
            .instance_env(e.index(), "portal")
            .and_then(|env| env.get::<String>("seen").ok())
            .unwrap_or_default();
        assert_eq!(env_seen, "arena/door2");
    }

    /// `lateUpdate` — the camera pass: runs when the driver says (after
    /// physics + writeback), sees the frame's dt, can move its node, and
    /// NEVER fires before the frame pass `start`ed the instance.
    #[test]
    fn late_update_runs_after_start_and_moves_the_node() {
        let dir = std::env::temp_dir().join("floptle_script_test_late");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "follow",
            "function update(node, dt)\n  node.y = 5\nend\n\
             function lateUpdate(node, dt)\n  node.x = node.x + dt\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "follow".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        // Before the frame pass builds+starts the instance, lateUpdate is a no-op.
        host.run_late(&mut world, 1.0, 0.0);
        assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 0.0);
        // A normal frame: update runs, then the driver's late pass.
        host.run(&mut world, &dir, 0.5, 0.5);
        host.run_late(&mut world, 0.5, 0.5);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let tr = world.get::<Transform>(e).unwrap();
        assert_eq!(tr.translation.y, 5.0, "update ran");
        assert!((tr.translation.x - 0.5).abs() < 1e-6, "lateUpdate moved the node by dt");
    }

    #[test]
    fn params_seeded_from_defaults_without_overrides() {
        // A script with `defaults` but NO per-instance overrides must still see params.X
        // (the bug: params was empty, so params.speed read nil).
        let dir = std::env::temp_dir().join("floptle_script_test_params_default");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "spin",
            "defaults = { speed = 90 }\nfunction update(node, dt)\n  node.yaw = node.yaw + math.rad(params.speed) * dt\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "spin".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0, 1.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let (yaw, _, _) = world.get::<Transform>(e).unwrap().rotation.to_euler(EulerRot::YXZ);
        assert!((yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-3, "params.speed default not applied; yaw {yaw}");
    }

    #[test]
    fn fixed_update_runs_per_tick_with_constant_dt() {
        // The gameplay-tick hook (docs/netcode-design.md §3): `fixedUpdate(node, dt)`
        // runs once per run_fixed call with the constant tick delta, only AFTER the
        // frame pass has started the script, and `update` does NOT run in the fixed
        // pass (nor fixedUpdate in the frame pass).
        let dir = std::env::temp_dir().join("floptle_script_test_fixed_update");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "ticker",
            "function update(node, dt)\n  node.y = node.y + 1\nend\n\
             function fixedUpdate(node, dt)\n  node.x = node.x + 1\n  node.z = dt\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "ticker".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
        );
        let mut host = ScriptHost::new();
        // run_fixed BEFORE any frame pass: instance doesn't exist yet → no tick, no error.
        host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 0.0);

        // One frame pass (start + update), then three fixed ticks.
        host.run(&mut world, &dir, 0.016, 0.016);
        for i in 0..3 {
            host.run_fixed(&mut world, 1.0 / 60.0, 0.016 + (i as f32) / 60.0);
        }
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let t = *world.get::<Transform>(e).unwrap();
        // x counts fixedUpdate calls (self-moves write back per tick); y counts updates.
        assert_eq!(t.translation.x, 3.0, "fixedUpdate must run once per run_fixed");
        assert_eq!(t.translation.y, 1.0, "update must run only in the frame pass");
        let want = (1.0f32 / 60.0) as f64;
        assert!((t.translation.z - want).abs() < 1e-9, "fixed dt must be the constant tick delta");
    }

    #[test]
    fn net_bridge_rpc_synced_events_round_trip() {
        // The Lua net.* bridge (docs/netcode-design.md §8): rpc queueing with
        // guardrails, replicated→synced declaration + collect/apply, onRpc
        // dispatch with sender, and net.on event handlers.
        let dir = std::env::temp_dir().join("floptle_script_test_net");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "netty",
            "replicated = { hp = 100, name = \"flop\" }\n\
             joined = 0\n\
             function start(node)\n  net.on(\"playerJoined\", function(p) joined = p end)\nend\n\
             function update(node, dt)\n\
               if time < 0.02 then\n\
                 net.rpc(\"hello\", { x = 1 })\n\
                 net.rpc(\"too_big\", string.rep(\"x\", 2000))\n\
               end\n\
             end\n\
             onRpc = {}\n\
             function onRpc.hurt(args, sender)\n  synced.hp = synced.hp - args.dmg\n  node.x = sender\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "netty".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
        );
        let mut host = ScriptHost::new();
        host.set_net_state(NetState {
            role: NetRoleState::Server,
            peers: vec![1],
            rtt_ms: 20.0,
            my_peer: None,
            ..Default::default()
        });
        host.run(&mut world, &dir, 0.01, 0.01);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

        // rpc queue: "hello" queued; the oversized one dropped with a warning.
        let cmds = host.take_net_commands();
        let rpcs: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                NetCmd::Rpc { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(rpcs, vec!["hello".to_string()], "guarded rpc must drop, got {cmds:?}");
        assert!(
            host.drain_logs().iter().any(|l| l.level == LogLevel::Warn && l.msg.contains("too_big")),
            "oversized rpc must warn"
        );

        // synced: declared values collected (sorted), server-side.
        let collected = host.collect_synced();
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].1, "netty");
        assert_eq!(
            collected[0].2,
            vec![
                ("hp".to_string(), floptle_net::NetValue::Num(100.0)),
                ("name".to_string(), floptle_net::NetValue::Str("flop".into())),
            ]
        );

        // onRpc dispatch mutates synced + gets the stamped sender.
        host.dispatch_rpc(
            &mut world,
            "hurt",
            &floptle_net::NetValue::Table(vec![(
                floptle_net::NetValue::Str("dmg".into()),
                floptle_net::NetValue::Num(25.0),
            )]),
            7,
        );
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let collected = host.collect_synced();
        assert_eq!(collected[0].2[0], ("hp".to_string(), floptle_net::NetValue::Num(75.0)));
        assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 7.0, "sender reaches Lua");

        // apply_synced (the client path) overwrites the store.
        host.apply_synced(e.index(), "netty", &[("hp".into(), floptle_net::NetValue::Num(10.0))]);
        let collected = host.collect_synced();
        assert_eq!(collected[0].2[0], ("hp".to_string(), floptle_net::NetValue::Num(10.0)));

        // net.on handler fires with the peer id.
        host.fire_net_event(&mut world, "playerJoined", Some(42), None);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        // (joined lives in the env; verify indirectly — no error + no crash is
        // the contract here; value-level checks ride the rpc/synced paths above.)

        // Client-side writes to synced warn.
        host.set_net_state(NetState { role: NetRoleState::Client, peers: vec![], rtt_ms: 0.0, my_peer: Some(7), ..Default::default() });
        host.dispatch_rpc(
            &mut world,
            "hurt",
            &floptle_net::NetValue::Table(vec![(
                floptle_net::NetValue::Str("dmg".into()),
                floptle_net::NetValue::Num(1.0),
            )]),
            0,
        );
        assert!(
            host.drain_logs().iter().any(|l| l.level == LogLevel::Warn && l.msg.contains("synced.hp")),
            "client synced write must warn"
        );
    }

    /// A game has to be able to tell its own players the lobby code.
    ///
    /// The relay hands it back at a moment only the engine sees, so a front end
    /// that couldn't read it had nowhere to get it — every game shipping a lobby
    /// screen had to send players to the engine's own debug panel to find out
    /// how their friends were supposed to join.
    #[test]
    fn a_lobby_screen_can_read_the_code_the_relay_handed_back() {
        let dir = std::env::temp_dir().join("floptle_script_test_lobby_code");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "lobby",
            "replicated = { code = \"\" }\n\
             function update(node, dt)\n  synced.code = net.lobbyCode() or \"waiting\"\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "lobby".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
        );
        let mut host = ScriptHost::new();

        // Before the relay answers: nil, so a lobby screen must poll rather
        // than read once. This is the state a host sits in for a round trip.
        host.set_net_state(NetState {
            role: NetRoleState::Server,
            peers: vec![],
            rtt_ms: 0.0,
            my_peer: None,
            ..Default::default()
        });
        host.run(&mut world, &dir, 0.016, 0.016);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let code = |h: &mut ScriptHost| match &h.collect_synced()[0].2[0].1 {
            floptle_net::NetValue::Str(s) => s.clone(),
            other => panic!("expected a string, got {other:?}"),
        };
        assert_eq!(code(&mut host), "waiting");

        // The relay answers.
        host.set_net_state(NetState {
            role: NetRoleState::Server,
            peers: vec![],
            rtt_ms: 0.0,
            my_peer: None,
            lobby_code: Some("QK7RM".into()),
            ..Default::default()
        });
        host.run(&mut world, &dir, 0.016, 0.032);
        assert_eq!(
            code(&mut host),
            "QK7RM",
            "the code must reach Lua, or a game cannot show it to the players who need it"
        );
    }

    /// A mistyped lobby code has to reach the player as words.
    ///
    /// It is the most common thing that will ever go wrong in an online
    /// session, and it used to arrive as an event indistinguishable from the
    /// opponent closing their laptop — the relay's own explanation was
    /// discarded one line below the pipe built to carry it.
    #[test]
    fn a_refused_join_reaches_the_game_with_the_relays_own_words() {
        let dir = std::env::temp_dir().join("floptle_script_test_join_state");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "lobby",
            "replicated = { shown = \"\" }\n\
             function update(node, dt)\n\
             \x20 local st, why = net.joinState()\n\
             \x20 synced.shown = why or st\n\
             end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "lobby".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
        );
        let mut host = ScriptHost::new();
        let shown = |h: &mut ScriptHost| match &h.collect_synced()[0].2[0].1 {
            floptle_net::NetValue::Str(s) => s.clone(),
            other => panic!("expected a string, got {other:?}"),
        };

        // Offline: no join in progress, and it says so rather than "".
        host.run(&mut world, &dir, 0.016, 0.016);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(shown(&mut host), "offline");

        // The join is in flight. This is the state a game used to be unable to
        // tell apart from success, because role already reads "client" here.
        host.set_net_state(NetState {
            role: NetRoleState::Client,
            join_state: "connecting",
            ..Default::default()
        });
        host.run(&mut world, &dir, 0.016, 0.032);
        assert_eq!(shown(&mut host), "connecting");

        // The relay answers. The game can print this.
        host.set_net_state(NetState {
            role: NetRoleState::Client,
            join_state: "refused",
            join_error: Some("no lobby QK7RM".into()),
            ..Default::default()
        });
        host.run(&mut world, &dir, 0.016, 0.048);
        assert_eq!(
            shown(&mut host),
            "no lobby QK7RM",
            "the relay's reason must reach Lua — without it a wrong code and a \
             dropped connection are the same event"
        );
    }

    #[test]
    fn predicted_node_update_rides_the_tick_clock() {
        // The anti-jitter contract (net play-as-client): a frame-filtered
        // entity's `update` is skipped in the per-frame pass and re-run at the
        // tick cadence via run_frame_for — so client and server integrate an
        // update-style controller identically. run_fixed_for also bypasses the
        // filters (it IS the substitute execution).
        let dir = std::env::temp_dir().join("floptle_script_test_frame_filter");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "mover", "function update(node, dt)\n  node.x = node.x + 1\nend\n");
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "mover".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.016, 0.016); // start + first update
        assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 1.0);

        let mut fskip = std::collections::HashSet::new();
        fskip.insert(e.index());
        host.set_frame_filter(fskip);
        host.run(&mut world, &dir, 0.016, 0.032); // frame pass: filtered → no move
        assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 1.0);
        host.run_frame_for(&mut world, e.index(), 1.0 / 60.0, 0.048); // tick-cadence update
        assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 2.0);

        host.set_frame_filter(std::collections::HashSet::new());
        host.run(&mut world, &dir, 0.016, 0.064); // cleared → frame pass runs again
        assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 3.0);
    }

    /// floptle/0042: a driver owns a node's TICKS, not its frames.
    ///
    /// `extend_filters` used to put the node in `script_skip`, which gates every
    /// pass — and the rollback driver replays only `fixedUpdate` and `update`.
    /// So `lateUpdate` had no substitute execution anywhere and simply stopped,
    /// with no error and no log line. It is the documented place to write a
    /// node's cosmetic transform (it runs after the interpolated writeback), so
    /// a game following that advice broke the instant the node went Rollback —
    /// and only in a net match, never offline.
    #[test]
    fn a_driver_owned_node_still_gets_its_late_pass() {
        let dir = std::env::temp_dir().join("floptle_script_test_driver_late");
        let _ = std::fs::create_dir_all(&dir);
        // Each pass counts itself in a distinct axis: x = update, y = fixedUpdate,
        // z = lateUpdate.
        write_script(
            &dir,
            "counter",
            "function update(node, dt)\n  node.x = node.x + 1\nend\n\
             function fixedUpdate(node, dt)\n  node.y = node.y + 1\nend\n\
             function lateUpdate(node, dt)\n  node.z = node.z + 1\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "counter".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        let pos = |w: &World| {
            let t = w.get::<Transform>(e).unwrap().translation;
            (t.x, t.y, t.z)
        };

        // Unclaimed: every pass runs globally.
        host.run(&mut world, &dir, 0.016, 0.016);
        host.run_fixed(&mut world, 0.016, 0.016);
        host.run_late(&mut world, 0.016, 0.016);
        assert_eq!(pos(&world), (1.0, 1.0, 1.0), "all three passes run when unclaimed");

        // The driver claims it. Its ticks move into the driver — but its late
        // pass has no substitute anywhere, so it must keep running here.
        host.extend_filters([e.index()]);
        assert!(host.is_filtered(e.index()), "the driver owns it");
        host.run(&mut world, &dir, 0.016, 0.032);
        host.run_fixed(&mut world, 0.016, 0.032);
        host.run_late(&mut world, 0.016, 0.032);
        assert_eq!(
            pos(&world),
            (1.0, 1.0, 2.0),
            "update/fixedUpdate are the driver's now; lateUpdate ran exactly once more"
        );

        // The driver's own substitute calls still bypass the filter.
        host.run_frame_for(&mut world, e.index(), 1.0 / 60.0, 0.048);
        host.run_fixed_for(&mut world, e.index(), 1.0 / 60.0, 0.048);
        assert_eq!(pos(&world), (2.0, 2.0, 2.0), "the driver replays the ticks it owns");

        // Released: back to every pass globally, exactly once.
        host.shrink_filters([e.index()]);
        assert!(!host.is_filtered(e.index()));
        host.run(&mut world, &dir, 0.016, 0.064);
        host.run_fixed(&mut world, 0.016, 0.064);
        host.run_late(&mut world, 0.016, 0.064);
        assert_eq!(pos(&world), (3.0, 3.0, 3.0), "handed back cleanly");
    }

    /// The OTHER reason a node is filtered must keep its old meaning: a
    /// snapshot-driven node is not simulated locally at all, so every pass —
    /// `lateUpdate` included — stays skipped. Separating the two sets must not
    /// leak the late pass into this case.
    #[test]
    fn a_snapshot_driven_node_still_skips_every_pass() {
        let dir = std::env::temp_dir().join("floptle_script_test_snapshot_late");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "late_only",
            "function lateUpdate(node, dt)\n  node.z = node.z + 1\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "late_only".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.016, 0.016);
        host.run_late(&mut world, 0.016, 0.016);
        assert_eq!(world.get::<Transform>(e).unwrap().translation.z, 1.0);

        let mut skip = std::collections::HashSet::new();
        skip.insert(e.index());
        host.set_script_filter(skip);
        host.run_late(&mut world, 0.016, 0.032);
        assert_eq!(
            world.get::<Transform>(e).unwrap().translation.z,
            1.0,
            "a server-authoritative node runs NO pass locally, late included"
        );
    }

    #[test]
    fn script_can_raycast() {
        let dir = std::env::temp_dir().join("floptle_script_test_raycast");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "caster",
            "function update(node, dt)\n  local h = raycast(0, 5, 0, 0, -1, 0, 20)\n  if h then node.y = h.y end\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "caster".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
        );
        let mut host = ScriptHost::new();
        host.set_colliders(
            vec![floptle_physics::AnchoredCollider::world(Box::new(floptle_physics::Plane::ground(0.0)))],
            glam::DVec3::ZERO,
        );
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let _ = host.take_colliders();
        let y = world.get::<Transform>(e).unwrap().translation.y;
        assert!(y.abs() < 0.1, "raycast should have set y to the ground (≈0), got {y}");
    }

    #[test]
    fn script_can_draw_gizmos() {
        let dir = std::env::temp_dir().join("floptle_script_test_gizmos");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "drawer",
            "function update(node, dt)\n  gizmo.line(0,0,0, 1,2,3)\n  gizmo.ray(0,0,0, 0,-2,0, 5, 1,0,0)\n  gizmo.sphere(4,5,6, 2)\n  gizmo.point(7,8,9)\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "drawer".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let cmds = host.take_gizmos();
        assert_eq!(cmds.len(), 4);
        // Explicit color sticks; ray normalizes the direction and scales by len.
        match cmds[1] {
            GizmoCmd::Line { a, b, color } => {
                assert_eq!(a, [0.0, 0.0, 0.0]);
                assert!((b[1] + 5.0).abs() < 1e-4, "ray end {b:?}");
                assert_eq!(color, [1.0, 0.0, 0.0]);
            }
            ref other => panic!("expected a line from gizmo.ray, got {other:?}"),
        }
        // Omitted color falls back to the default green.
        match cmds[0] {
            GizmoCmd::Line { color, .. } => assert!(color[1] > 0.9),
            ref other => panic!("expected a line, got {other:?}"),
        }
        // A second run() starts a fresh (empty) batch — immediate mode.
        host.run(&mut world, &dir, 0.1, 0.2);
        assert_eq!(host.take_gizmos().len(), 4);
    }

    #[test]
    fn preprocess_rewrites_compound_ops() {
        assert_eq!(preprocess("x += y"), "x = x + (y)");
        assert_eq!(preprocess("tbl.k *= 2"), "tbl.k = tbl.k * (2)");
        assert_eq!(preprocess("a[i] -= f()"), "a[i] = a[i] - (f())");
        assert_eq!(preprocess("s ..= 'z'"), "s = s .. ('z')");
        assert_eq!(preprocess("p %= 3"), "p = p % (3)");
        assert_eq!(preprocess("q ^= 2"), "q = q ^ (2)");
        assert_eq!(preprocess("n /= 2"), "n = n / (2)");
        // Precedence: the whole RHS is parenthesized.
        assert_eq!(preprocess("x *= a + b"), "x = x * (a + b)");
        // Nested index lvalue, balanced brackets.
        assert_eq!(preprocess("a[b[i]] += 1"), "a[b[i]] = a[b[i]] + (1)");
        // Inline block (lvalue back-scan stops at the keyword boundary).
        assert_eq!(preprocess("if c then x += 1 end"), "if c then x = x + (1) end");
    }

    #[test]
    fn preprocess_ignores_strings_and_comments() {
        assert_eq!(preprocess("s = 'x += y'"), "s = 'x += y'");
        assert_eq!(preprocess("-- x += y"), "-- x += y");
        assert_eq!(preprocess("t = [[ a += b ]]"), "t = [[ a += b ]]");
        assert_eq!(preprocess("t = [==[ a += b ]==]"), "t = [==[ a += b ]==]");
        assert_eq!(preprocess("if a == b then end"), "if a == b then end");
        assert_eq!(preprocess("c = a .. b"), "c = a .. b"); // concat untouched
        assert_eq!(preprocess("x = -y"), "x = -y"); // unary minus untouched
    }

    #[test]
    fn preprocess_preserves_line_count() {
        let src = "x += 1\ny -= 2\n-- z += 3\n";
        assert_eq!(preprocess(src).matches('\n').count(), src.matches('\n').count());
    }

    #[test]
    fn preprocess_closes_rhs_at_comments_and_statements() {
        // Trailing comment must not be swallowed into the RHS parentheses.
        assert_eq!(preprocess("x += 1 -- note"), "x = x + (1) -- note");
        assert_eq!(preprocess("s ..= 'z' -- c"), "s = s .. ('z') -- c");
        // A call/parenthesized receiver lvalue is captured whole.
        assert_eq!(preprocess("f().x += 1"), "f().x = f().x + (1)");
        assert_eq!(preprocess("(a).b -= 2"), "(a).b = (a).b - (2)");
        // A statement-introducing keyword on the same line terminates the RHS.
        assert_eq!(
            preprocess("function f() x += 1 return x end"),
            "function f() x = x + (1) return x end"
        );
        assert_eq!(preprocess("while c do n += 1 end"), "while c do n = n + (1) end");
    }

    #[test]
    fn compound_assignment_runs_end_to_end() {
        let dir = std::env::temp_dir().join("floptle_script_test_compound");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "spin",
            "defaults = { speed = 90 }\nfunction update(node, dt)\n  node.yaw += math.rad(params.speed) * dt\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Scripts(vec![floptle_core::ScriptInst {
            kind: "spin".into(),
            enabled: true,
            params: vec![("speed".into(), 90.0)], refs: Vec::new(),
            strs: Vec::new(),
        }]));
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0, 1.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let tr = world.get::<Transform>(e).unwrap();
        let (yaw, _, _) = tr.rotation.to_euler(EulerRot::YXZ);
        assert!((yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-3, "yaw was {yaw}");
    }

    #[test]
    fn script_reads_grounded_and_writes_velocity() {
        // The physics API: a script reads node.grounded + sets node.vx; the engine
        // reads that velocity back via take_body_changes.
        let dir = std::env::temp_dir().join("floptle_script_test_physapi");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "drive",
            "function update(node, dt)\n  if node.grounded then node.vx = 5.0 end\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Scripts(vec![floptle_core::ScriptInst {
            kind: "drive".into(),
            enabled: true,
            params: Vec::new(), refs: Vec::new(),
            strs: Vec::new(),
        }]));
        let mut host = ScriptHost::new();
        let mut bodies = HashMap::new();
        bodies.insert(
            e.index(),
            BodyState { grounded: true, ..Default::default() },
        );
        host.set_bodies(bodies);
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let changes = host.take_body_changes();
        assert_eq!(changes.get(&e.index()).copied().unwrap()[0], 5.0);
    }

    #[test]
    fn defaults_are_read() {
        let dir = std::env::temp_dir().join("floptle_script_test_defaults");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "pulsate", "defaults = { amplitude = 0.3, speed = 2.0, base = 1.0 }\n");
        let host = ScriptHost::new();
        let (d, refs, _strs) = host.script_defaults(&dir.join("pulsate.lua"));
        assert_eq!(d.len(), 3);
        assert!(refs.is_empty());
        assert!(d.iter().any(|(k, v)| k == "amplitude" && (*v - 0.3).abs() < 1e-6));
    }

    fn world_with_script(kind: &str) -> (World, Entity) {
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Scripts(vec![floptle_core::ScriptInst {
            kind: kind.into(),
            enabled: true,
            params: vec![], refs: Vec::new(),
            strs: Vec::new(),
        }]));
        (world, e)
    }

    /// A cross-script `h.name(...)` calls the script's own function
    /// (`floptle/0085`).
    ///
    /// This is the exact shape a player reported as "the commerce center is
    /// still just erroring": `materials.lua` exported `function name(id)`
    /// returning a display name — the obvious spelling — and the handle answered
    /// `name` itself, with the script's own kind, as a string. Every caller died
    /// at the call site with `attempt to call field 'name' (a string value)`,
    /// and only at the moment it had something to display, so the mining
    /// readout, the pickup line, the depot stock list and the research panel
    /// each broke separately.
    #[test]
    fn a_script_that_exports_name_can_be_called_by_other_scripts() {
        let dir = std::env::temp_dir().join(format!("floptle_shadow_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "materials",
            "\
function name(id)
  return id == 'iron' and 'Iron Ore' or '?'
end
",
        );
        write_script(
            &dir,
            "readout",
            "\
function update(node, dt)
  local m = findScript('materials')
  label = m.name('iron')
  which = m.kind
  live = m.valid
end
",
        );
        let (mut world, e) = world_with_script("readout");
        let mats = world.spawn();
        world.insert(mats, Transform::IDENTITY);
        world.insert(mats, floptle_core::Name("Materials".into()));
        world.insert(
            mats,
            floptle_core::Scripts(vec![floptle_core::ScriptInst::new("materials")]),
        );
        let mut host = ScriptHost::new();
        // Two frames: the readout may reach the materials handle only once both
        // instances have been ensured.
        for i in 0..2 {
            host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
        }
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let env = host.instance_env(e.index(), "readout").expect("the readout ran");
        assert_eq!(
            env.get::<String>("label").ok().as_deref(),
            Some("Iron Ore"),
            "the handle answered `name` itself instead of calling the script's function"
        );
        // …and the two keys that ARE the handle's still work, so nothing lost the
        // ability to ask which script a handle is or whether it is still loaded.
        assert_eq!(env.get::<String>("which").ok().as_deref(), Some("materials"));
        assert_eq!(env.get::<bool>("live").ok(), Some(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A script exporting a name the handle DOES keep is reported at load, once,
    /// naming the script and the key (`floptle/0085`).
    #[test]
    fn exporting_a_reserved_handle_key_is_reported_at_load() {
        let dir = std::env::temp_dir().join(format!("floptle_shadow2_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "stock",
            "\
function kind(id)
  return 'ore'
end

function update(node, dt)
  ran = true
end
",
        );
        let (mut world, _e) = world_with_script("stock");
        let mut host = ScriptHost::new();
        for i in 0..3 {
            host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
        }
        let logs = host.drain_logs();
        let warns: Vec<&crate::ScriptLog> = logs
            .iter()
            .filter(|l| l.level == crate::LogLevel::Warn && l.msg.contains("findScript handle"))
            .collect();
        assert_eq!(warns.len(), 1, "one line per script per session: {warns:?}");
        let msg = &warns[0].msg;
        for want in ["stock", "`kind`", "which script this is"] {
            assert!(msg.contains(want), "missing {want:?}: {msg}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A script points a camera at a render target and reads back what it got
    /// (`floptle/0078`).
    ///
    /// The camera group had seven entries and not one of them rendered
    /// anything: `target` was settable only in the Inspector, so a minimap was
    /// impossible from script even though the engine had been rendering camera
    /// targets for two releases.
    #[test]
    fn a_script_aims_a_camera_at_a_render_target_and_sizes_it() {
        let dir = std::env::temp_dir().join(format!("floptle_rt_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "minimap",
            "\
function start(node)
  local eye = find('MapEye')
  eye:setCamera{ target = 'minimap', width = 256, height = 256, hz = 10, fovY = 1.2 }
  node:setMaterial{ texture = 'rt:minimap', unlit = true }
end
",
        );
        let (mut world, e) = world_with_script("minimap");
        // The camera is a scene node the script finds and aims, which is how a
        // game's minimap camera is authored: once, then driven.
        let eye = world.spawn();
        world.insert(eye, Transform::IDENTITY);
        world.insert(eye, floptle_core::Name("MapEye".into()));
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        // The screen wears the live feed.
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let m = world.get::<floptle_core::Material>(e).expect("setMaterial ran");
        assert_eq!(m.texture.as_deref(), Some("rt:minimap"));
        // The camera the script created carries its own size and rate — not the
        // 480×270-every-frame every target used to get.
        let cam = world
            .query::<Matter>()
            .find_map(|(_, m)| match m {
                Matter::Camera { target, target_w, target_h, target_hz, fov_y, .. }
                    if target == "minimap" =>
                {
                    Some((*target_w, *target_h, *target_hz, *fov_y))
                }
                _ => None,
            })
            .expect("setCamera made a render-target camera");
        assert_eq!((cam.0, cam.1), (256, 256), "the size the script asked for");
        assert_eq!(cam.2, 10.0, "the rate the script asked for");
        assert!((cam.3 - 1.2).abs() < 1e-5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every way of getting `setCamera` wrong raises AT THE CALL, naming the
    /// property, the value and what is accepted (`floptle/0082`).
    ///
    /// A silently-defaulted render target is invisible: the texture resolves,
    /// the picture is there, and it is simply the wrong size or rate forever.
    #[test]
    fn a_bad_camera_option_is_refused_where_it_was_written() {
        let dir = std::env::temp_dir().join(format!("floptle_rt_bad_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // (what the script writes, what the message must contain)
        let cases: &[(&str, &[&str])] = &[
            ("node:setCamera{ targt = 'minimap' }", &["targt", "did you mean `target`"]),
            ("node:setCamera{ width = 0 }", &["width", "8"]),
            ("node:setCamera{ hz = '10' }", &["hz", "string"]),
            ("node:setCamera{ active = 0 }", &["active", "true or false"]),
            ("node:setCamera{ target = 42 }", &["target", "integer"]),
            // The prefix belongs to the texture ref, not to the name — this
            // would otherwise make a texture called `rt:rt:minimap`, which
            // resolves to nothing and says nothing.
            ("node:setCamera{ target = 'rt:minimap' }", &["rt:minimap", "target = \"minimap\""]),
        ];
        for (i, (src, wants)) in cases.iter().enumerate() {
            let name = format!("bad{i}");
            write_script(&dir, &name, &format!("function start(node)\n  {src}\nend\n"));
            let (mut world, _e) = world_with_script(&name);
            let mut host = ScriptHost::new();
            host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
            let errs = host.errors().to_vec();
            assert!(!errs.is_empty(), "`{src}` was accepted silently");
            let msg = errs.join(" | ");
            for want in *wants {
                assert!(msg.contains(want), "`{src}` error is missing {want:?}: {msg}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The 2D layer, end to end from Lua (`floptle/0058`): build a grid, paint
    /// squares, read them back, and draw sprites into a batch.
    #[test]
    fn a_script_builds_a_tilemap_and_fills_a_sprite_batch() {
        let dir = std::env::temp_dir().join(format!("floptle_2d_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "flat",
            "\
function start(node)
  node:setTilemap{ cols = 4, rows = 3, tile = 2.0 }
end

function update(node, dt)
  local tm = node:tilemap()
  tm:fill(1)
  tm:set(0, 0, 7)
  tm:set(3, 2, 9)
  tm:set(99, 99, 5)      -- outside the grid: a no-op, not a wrap
  readBack = tm:get(0, 0)
  cols, rows = tm:size()

  -- A node is not a sprite batch until it is told to be one, and taking the
  -- handle in the very next line has to work (`floptle/0062`). A separate node
  -- because Matter is exclusive: a tilemap is not also a batch.
  local nd = find('Batch')
  nd:setSpriteBatch{ size = 1.0 }
  local b = nd:sprites()
  b:draw(1, 2)                                   -- the short form
  b:draw(3, 4, 0, 2.0, 1.5, 6, 1, 0.2, 0.2, 0.5) -- …and the whole thing
  b:draw(5, 6, 0, vec2(1.4, 0.6))                -- squash and stretch
end
",
        );
        let (mut world, e) = world_with_script("flat");
        world.insert(e, floptle_core::Matter::Empty);
        let batch = world.spawn();
        world.insert(batch, Transform::IDENTITY);
        world.insert(batch, floptle_core::Name("Batch".into()));
        world.insert(batch, floptle_core::Matter::Empty);
        let mut host = ScriptHost::new();
        // Two passes: `start` builds the grid, and the writes queued in the
        // first `update` land before the second reads them back.
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        assert!(host.errors().is_empty(), "{:?}", host.errors());

        let Some(floptle_core::Matter::Tilemap { cols, rows, tile, data, .. }) =
            world.get::<floptle_core::Matter>(e)
        else {
            panic!("setTilemap did not make a tilemap: {:?}", world.get::<floptle_core::Matter>(e))
        };
        assert_eq!((*cols, *rows, *tile), (4, 3, 2.0));
        assert_eq!(data.len(), 12, "the grid is sized even with no data given");
        assert_eq!(data[0], 7, "tm:set(0, 0, ..) writes the top-left");
        assert_eq!(data[11], 9, "…and (3, 2) the bottom-right");
        assert_eq!(data[1], 1, "tm:fill covered the rest");
        assert!(data.iter().all(|c| *c != 5), "an out-of-bounds set must not wrap");

        // `setSpriteBatch` made the OTHER node a batch, from Lua alone.
        assert!(
            matches!(
                world.get::<floptle_core::Matter>(batch),
                Some(floptle_core::Matter::SpriteBatch { .. })
            ),
            "setSpriteBatch made it a batch: {:?}",
            world.get::<floptle_core::Matter>(batch)
        );
        host.run(&mut world, &dir, 1.0 / 60.0, 2.0 / 60.0);
        let sprites = world.get::<floptle_core::Sprites>(batch).expect("sprites");
        assert_eq!(sprites.0.len(), 3, "three draws, three sprites");
        assert_eq!(sprites.0[0].pos, [1.0, 2.0, 0.0]);
        assert_eq!(sprites.0[0].tint, [1.0, 1.0, 1.0, 1.0], "the short form is untinted");
        assert_eq!(sprites.0[0].scale, [1.0, 1.0], "…and unscaled");
        assert_eq!(sprites.0[1].cell, 6);
        assert_eq!(sprites.0[1].tint, [1.0, 0.2, 0.2, 0.5], "the per-sprite tint survives");
        assert_eq!(sprites.0[1].scale, [2.0, 2.0], "one number scales both axes");
        assert_eq!(sprites.0[2].scale, [1.4, 0.6], "…and a vec2 stretches one of them");

        // IMMEDIATE MODE: a pass that draws nothing leaves nothing behind.
        write_script(&dir, "flat", "function update(node, dt)\nend\n");
        host.run(&mut world, &dir, 1.0 / 60.0, 3.0 / 60.0);
        assert!(
            world.get::<floptle_core::Sprites>(batch).is_some_and(|s| s.0.is_empty()),
            "sprites must not survive a frame nobody drew them"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A script sees its OWN cost, attributed by file name (`floptle/0077`).
    ///
    /// End to end through the real host, because the value of this API is
    /// entirely in a game being able to assert its own budget — and the thing
    /// that makes that possible is per-script attribution, which nothing outside
    /// `run_pass` can produce.
    #[test]
    fn a_script_reads_its_own_frame_cost_by_name() {
        let dir = std::env::temp_dir().join(format!("floptle_perf_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // Two scripts, one deliberately doing more work than the other, so the
        // ordering `perf.scripts()` promises has something to order.
        write_script(
            &dir,
            "busy",
            "\
function start(node)
  perf.enable(true)
end

function update(node, dt)
  local acc = 0
  for i = 1, 200000 do acc = acc + i % 7 end
  spun = acc
end
",
        );
        write_script(&dir, "idle", "function update(node, dt)\n  ticked = true\nend\n");
        let (mut world, _e) = world_with_script("busy");
        let idle = world.spawn();
        world.insert(idle, Transform::IDENTITY);
        world.insert(idle, floptle_core::Name("Idle".into()));
        world.insert(
            idle,
            floptle_core::Scripts(vec![floptle_core::ScriptInst::new("idle")]),
        );
        let mut host = ScriptHost::new();
        // `start` turns collection on; the frames after it are the measured ones.
        for i in 0..4 {
            host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
            host.profile().borrow_mut().end_frame();
        }
        let prof = host.profile().borrow();
        assert!(prof.enabled(), "the script's own perf.enable(true) did not take");
        let rows = prof.scripts();
        assert!(rows.len() >= 2, "both scripts should be listed: {rows:?}");
        assert_eq!(rows[0].0, "busy", "most expensive first: {rows:?}");
        assert!(rows[0].1.ms > 0.0, "the busy script measured as free: {rows:?}");
        // The bucket is the rows added up, so the readout cannot disagree with
        // itself — that discrepancy is what makes a reader stop trusting one.
        let bucket = prof.bucket(floptle_core::profile::Bucket::Scripts).expect("on");
        let sum: f32 = rows.iter().map(|(_, c)| c.ms).sum();
        assert!(
            (bucket.ms - sum).abs() < sum.max(0.001) * 0.05,
            "bucket {} vs rows {sum}",
            bucket.ms
        );
        drop(prof);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Polling a key the host keeps says so, once, instead of reading `false`
    /// forever (`floptle/0084`).
    ///
    /// The failure this replaces has no symptom: `input.pressed(k)` returning
    /// false is what a key nobody pressed also looks like, so there is nothing
    /// to log, nothing to assert and nothing to fall back to from inside the
    /// game. One Console line is the whole difference between a five-second fix
    /// and a player telling you your feature does not exist.
    #[test]
    fn polling_a_reserved_key_warns_once_and_says_what_takes_it() {
        let dir = std::env::temp_dir().join(format!("floptle_reserved_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "hotkey",
            "\
function update(node, dt)
  if input.pressed('f1') then end
  if input.key('F1') then end      -- same key, different spelling
  if input.pressed('i') then end   -- a key the game actually gets
end
",
        );
        let (mut world, _e) = world_with_script("hotkey");
        let mut host = ScriptHost::new();
        host.set_reserved_keys(&[("f1", "Play / Stop")]);
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        let warns: Vec<String> = host
            .drain_logs()
            .into_iter()
            .filter(|l| l.level == LogLevel::Warn)
            .map(|l| l.msg)
            .collect();
        assert_eq!(warns.len(), 1, "one line per key, not one per poll: {warns:?}");
        assert!(warns[0].contains("f1"), "names the key: {}", warns[0]);
        assert!(warns[0].contains("Play / Stop"), "names what takes it: {}", warns[0]);

        // …and it does not repeat every frame. A warning that floods the Console
        // is a warning that gets scrolled past.
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        assert!(
            host.drain_logs().iter().all(|l| l.level != LogLevel::Warn),
            "the same key warned twice"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With nothing reserved — a headless harness, or a build that keeps no keys
    /// — the check is silent. The default must not invent warnings.
    #[test]
    fn nothing_is_reserved_by_default() {
        let dir = std::env::temp_dir().join(format!("floptle_unreserved_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "poll", "function update(node, dt)\n  if input.pressed('f1') then end\nend\n");
        let (mut world, _e) = world_with_script("poll");
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.drain_logs().iter().all(|l| l.level != LogLevel::Warn));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The 2D surface a real tilemap game reaches for, end to end through Lua.
    ///
    /// Every one of these was hand-rolled in both in-house games before it
    /// existed, and each hand-rolled copy was wrong in the same way: it
    /// duplicated the grid's centring and its row-0-is-the-top convention, and
    /// went stale the moment the map was moved. So the test that matters is not
    /// "does `set` write a square" — it is "does the WORLD conversion survive the
    /// node's transform", which is the part a script cannot check for itself.
    #[test]
    fn a_script_can_place_read_and_locate_tiles_through_the_handle() {
        let dir = std::env::temp_dir().join(format!("floptle_tiles2d_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "level",
            "\
function start(node)
  node:setTilemap{ cols = 4, rows = 3, tile = 2.0 }
  local tm = node:tilemap()
  tm:fill(0)
  -- A turned tile: rot is degrees clockwise, and the pair reads back canonically.
  tm:set(1, 1, 5, { rot = 90 })
  tm:set(2, 1, 6, { flipX = true })
  -- A rectangle, corners in either order, clipped at the edge.
  tm:fillRect(3, 0, 9, 9, 7)
end

-- The reads live in `update`, because the construction API is DEFERRED: what
-- `start` queued lands in the flush after it, and the scene mirror a handle
-- reads is rebuilt at the top of the next pass. A game reading back its own
-- writes in the same hook is reading the frame before.
function update(node, dt)
  local tm = node:tilemap()
  cols, rows = tm:size()
  edge = tm:tileSize()
  cellAt11, xf11, flip11 = tm:at(1, 1)
  plainGet = tm:get(1, 1)
  -- World <-> cell, through the node's own transform.
  local c = tm:worldAt(0, 0)
  cx, cy = tm:cellAt(c)
  -- …and a point well outside the map is off the map, not clamped to an edge.
  offX, offY = tm:cellAt(vec3(1000, 0, 0))
  clipped = tm:get(3, 2)
end
",
        );
        let (mut world, e) = world_with_script("level");
        world.insert(e, floptle_core::Matter::Empty);
        // A MOVED, TURNED and SCALED map — the case a Lua copy of the maths gets
        // wrong. If `cellAt(worldAt(0, 0))` still comes back (0, 0) here, the
        // conversion is going through the transform rather than assuming
        // identity.
        world.insert(
            e,
            Transform {
                translation: glam::DVec3::new(37.0, -12.0, 4.0),
                rotation: glam::Quat::from_rotation_z(0.7),
                scale: glam::Vec3::new(1.5, 1.5, 1.0),
            },
        );
        let mut host = ScriptHost::new();
        // Two passes: `start` queues the construction writes, and the second run
        // re-mirrors the scene so the reads see them.
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        assert!(host.errors().is_empty(), "{:?}", host.errors());

        let env = host.instance_env(e.index(), "level").expect("the script ran");
        let num = |k: &str| env.get::<f64>(k).unwrap_or(-999.0);
        assert_eq!((num("cols"), num("rows")), (4.0, 3.0));
        assert_eq!(num("edge"), 2.0, "tm:tileSize is the world edge of one square");

        assert_eq!(num("cellAt11"), 5.0, "tm:at gives the cell");
        assert_eq!(num("xf11"), 90.0, "…and the rotation, in degrees clockwise");
        assert_eq!(num("plainGet"), 5.0, "tm:get strips the orientation");
        assert!(
            !env.get::<bool>("flip11").unwrap_or(true),
            "a pure rotation is not mirrored"
        );

        assert_eq!(
            (num("cx"), num("cy")),
            (0.0, 0.0),
            "worldAt then cellAt must round-trip THROUGH the node's transform"
        );
        assert!(
            env.get::<mlua::Value>("offX").map(|v| v.is_nil()).unwrap_or(false),
            "a point off the map is nil, not clamped to an edge square"
        );
        assert_eq!(num("clipped"), 7.0, "the rectangle clipped to the grid and filled the corner");

        // The component itself carries the packed orientation, so a saved scene
        // records it and the mesh draws it.
        let Some(floptle_core::Matter::Tilemap { data, .. }) =
            world.get::<floptle_core::Matter>(e)
        else {
            panic!("setTilemap did not make a tilemap")
        };
        // row 1, column 1 of a 4-wide grid.
        let turned = data[4 + 1];
        assert_eq!(floptle_core::tile_index(turned), 5);
        assert_eq!(floptle_core::tile_xform(turned), floptle_core::TileXform::new(1, false));
        let mirrored = data[4 + 2];
        assert_eq!(floptle_core::tile_index(mirrored), 6);
        assert!(floptle_core::tile_xform(mirrored).flip_x, "flipX = true must mirror it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A wrong orientation is refused where it was written, not rounded down to
    /// something that looks almost right (`floptle/0082`).
    #[test]
    fn a_bad_tile_orientation_is_refused_at_the_call() {
        let dir = std::env::temp_dir().join(format!("floptle_tilexf_bad_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cases: &[(&str, &[&str])] = &[
            // 45 degrees is not one of the eight things a square tile can be.
            ("tm:set(0, 0, 1, { rot = 45 })", &["rot = 45", "quarter-turns"]),
            // A typo in an orientation key is a tile placed unturned, silently.
            ("tm:set(0, 0, 1, { flipx = true })", &["flipx", "did you mean `flipX`"]),
            // A resize with nothing to resize to is a mistake, not a no-op.
            ("tm:resize{}", &["cols", "rows"]),
            ("tm:resize{ colls = 4 }", &["colls", "did you mean `cols`"]),
        ];
        for (i, (src, wants)) in cases.iter().enumerate() {
            let name = format!("tbad{i}");
            write_script(
                &dir,
                &name,
                &format!(
                    "function start(node)\n  node:setTilemap{{ cols = 2, rows = 2 }}\n  \
                     local tm = node:tilemap()\n  {src}\nend\n"
                ),
            );
            let (mut world, e) = world_with_script(&name);
            world.insert(e, floptle_core::Matter::Empty);
            let mut host = ScriptHost::new();
            host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
            let errs = host.errors().to_vec();
            assert!(!errs.is_empty(), "`{src}` was accepted silently");
            let msg = errs.join(" | ");
            for want in *wants {
                assert!(msg.contains(want), "`{src}` error is missing {want:?}: {msg}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The arena loop that shipped with its walls missing now runs to the end
    /// (`floptle/0083`).
    ///
    /// This is the real thing, not a unit test of the converter: a wall tilemap
    /// written row by row with the play area punched out of the middle, and a
    /// line AFTER the loop that has to be reached. `-1` used to fail the `u32`
    /// conversion and raise, so the loop died on the first inside square — two
    /// rows in — the mesh kept its padding, and the node never reached the line
    /// that positions it. What the player saw was "the walls are not visible".
    ///
    /// The `EMPTY_TILE` global is checked in the same pass because it is what
    /// the editor's autocomplete has told people to write since tilemaps
    /// shipped; before this it resolved to `nil`, which then also failed to
    /// convert.
    #[test]
    fn punching_a_hole_in_a_wall_tilemap_runs_to_the_end_of_the_loop() {
        let dir = std::env::temp_dir().join(format!("floptle_empty_tile_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "arena",
            "\
function start(node)
  local gw, gh, band = 5, 5, 1
  node:setTilemap{ cols = gw, rows = gh, tile = 1.0 }
  local tm = node:tilemap()
  for gy = 0, gh - 1 do
    for gx = 0, gw - 1 do
      local inside = gx >= band and gx < gw - band and gy >= band and gy < gh - band
      tm:set(gx, gy, inside and -1 or 4)
    end
  end
  -- The three other spellings of empty all have to reach the same value.
  tm:set(2, 2, EMPTY_TILE)
  tm:set(3, 2, tm.EMPTY)
  tm:set(1, 3, nil)
  -- The line after the loop. This is the one the raise used to eat.
  reachedTheEnd = true
end
",
        );
        let (mut world, e) = world_with_script("arena");
        world.insert(e, floptle_core::Matter::Empty);
        let mut host = ScriptHost::new();
        // `start` queues the grid and the writes; the second pass applies them.
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        assert!(host.errors().is_empty(), "a negative cell must not raise: {:?}", host.errors());

        let Some(floptle_core::Matter::Tilemap { data, .. }) =
            world.get::<floptle_core::Matter>(e)
        else {
            panic!("no tilemap")
        };
        // The border is wall on every side — the loop got all the way round,
        // rather than dying on the first square that wanted to be empty.
        for (i, cell) in data.iter().enumerate() {
            let (x, y) = (i as u32 % 5, i as u32 / 5);
            let border = x == 0 || y == 0 || x == 4 || y == 4;
            let want = if border { 4 } else { floptle_core::EMPTY_TILE };
            assert_eq!(*cell, want, "cell ({x}, {y}) is wrong: the play area is the empty part");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sprite survives to the end of the FRAME whichever pass drew it
    /// (`floptle/0070`).
    ///
    /// The batches used to be emptied after every pass, so the fixed pass wiped
    /// whatever `update` drew and the late pass wiped that — leaving `lateUpdate`
    /// as the only place a draw survived, silently, with no error and nothing on
    /// screen. `update` is where every tutorial puts per-frame work and where
    /// `draw.*` goes, so the one obvious spelling was the one that could not work.
    #[test]
    fn a_sprite_drawn_in_any_pass_survives_the_frame() {
        let dir = std::env::temp_dir().join(format!("floptle_0070_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "render",
            "\
frames = 0
function start(node)
  node:setSpriteBatch{ size = 1.0 }
  find('Fixed'):setSpriteBatch{ size = 1.0 }
  find('Late'):setSpriteBatch{ size = 1.0 }
end

-- One batch per pass, so a wipe by a LATER pass is visible as an empty list
-- rather than hidden by the next pass redrawing the same thing.
function update(node, dt)
  frames = frames + 1
  if frames > 2 then return end     -- …and then the game stops drawing entirely
  node:sprites():draw(1, 1)
  node:sprites():draw(2, 2)
end

function fixedUpdate(node, dt)
  if frames > 2 then return end
  find('Fixed'):sprites():draw(3, 3)
end

function lateUpdate(node, dt)
  if frames > 2 then return end
  find('Late'):sprites():draw(4, 4)
end
",
        );
        let (mut world, e) = world_with_script("render");
        world.insert(e, floptle_core::Name("Frame".into()));
        world.insert(e, floptle_core::Matter::Empty);
        let mut named = |n: &str| {
            let b = world.spawn();
            world.insert(b, Transform::IDENTITY);
            world.insert(b, floptle_core::Name(n.into()));
            world.insert(b, floptle_core::Matter::Empty);
            b
        };
        let fixed = named("Fixed");
        let late = named("Late");

        let mut host = ScriptHost::new();
        let count = |world: &World, b| {
            world.get::<floptle_core::Sprites>(b).map(|s| s.0.len()).unwrap_or(0)
        };
        // The driver's whole frame, in its real order.
        for f in 0..2 {
            let t = f as f32 / 60.0;
            host.run(&mut world, &dir, 1.0 / 60.0, t);
            host.run_fixed(&mut world, 1.0 / 60.0, t);
            host.run_late(&mut world, 1.0 / 60.0, t);
            assert!(host.errors().is_empty(), "{:?}", host.errors());

            assert_eq!(count(&world, e), 2, "frame {f}: the `update` draws survived to the end");
            assert_eq!(count(&world, fixed), 1, "frame {f}: so did the `fixedUpdate` draw");
            assert_eq!(count(&world, late), 1, "frame {f}: and the `lateUpdate` draw");
        }

        // Still immediate mode: the frame is the unit, so a frame nobody draws
        // in clears every batch — no pool to grow, nothing to `clear()`.
        host.run(&mut world, &dir, 1.0 / 60.0, 2.0 / 60.0);
        host.run_fixed(&mut world, 1.0 / 60.0, 2.0 / 60.0);
        host.run_late(&mut world, 1.0 / 60.0, 2.0 / 60.0);
        for (b, who) in [(e, "update"), (fixed, "fixedUpdate"), (late, "lateUpdate")] {
            assert_eq!(count(&world, b), 0, "a frame with no draws empties the {who} batch");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A scene of thousands of scripted nodes runs (`floptle/0069`).
    ///
    /// It used to PANIC — `out of auxiliary stack space (used 7999 slots)` —
    /// because the host held a live `mlua::Table` per instance in two places,
    /// and each one costs a slot on a ref stack bounded near 8,000. Two holds
    /// put the ceiling around four thousand, which a probe hit and a game
    /// eventually would have. Registry keys have no such bound.
    ///
    /// 6,000 because it is comfortably past the old ceiling while staying a
    /// second-ish test; `examples/auxstack_probe` runs it to 20,000 and shows
    /// which of the two ways of holding a Lua value is the one that runs out.
    #[test]
    fn thousands_of_scripted_nodes_do_not_exhaust_lua() {
        let dir = std::env::temp_dir().join(format!("floptle_0069_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "prop", "n = 0\nfunction update(node, dt)\n  n = n + 1\nend\n");

        const NODES: usize = 6_000;
        let mut world = World::default();
        let mut last = None;
        for i in 0..NODES {
            let e = world.spawn();
            world.insert(e, Transform::IDENTITY);
            world.insert(e, floptle_core::Name(format!("prop{i}")));
            world.insert(e, Scripts(vec![floptle_core::ScriptInst {
                kind: "prop".into(),
                enabled: true,
                params: Vec::new(),
                refs: Vec::new(),
                strs: Vec::new(),
            }]));
            last = Some(e);
        }
        let mut host = ScriptHost::new();
        // Two frames: the first builds every environment, the second proves they
        // are all still reachable — a registry key that was dropped on the way in
        // would read as a script that silently stopped running.
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        assert!(host.errors().is_empty(), "{:?}", host.errors());

        let last = last.expect("nodes");
        let env = host.instance_env(last.index(), "prop").expect("the LAST instance still resolves");
        assert_eq!(
            env.get::<f64>("n").unwrap(),
            2.0,
            "every instance ran both frames, including the ones past the old ceiling"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A scene param the script no longer declares is stored and never read —
    /// and from the outside that is indistinguishable from a script whose
    /// numbers do nothing (`floptle/0068`). One line, once per session.
    #[test]
    fn a_scene_param_the_script_does_not_declare_says_so_once() {
        let dir = std::env::temp_dir().join(format!("floptle_0068_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "tool", "defaults = { reach = 4.0 }\nfunction update(node, dt)\nend\n");
        let mut world = World::default();
        // Three instances of the same script, all carrying the stale param —
        // eighteen `sas_button`s must not be eighteen identical lines.
        for i in 0..3 {
            let e = world.spawn();
            world.insert(e, Transform::IDENTITY);
            world.insert(e, floptle_core::Name(format!("Belt{i}")));
            world.insert(e, Scripts(vec![floptle_core::ScriptInst {
                kind: "tool".into(),
                enabled: true,
                params: vec![("reach".into(), 6.0), ("laser_range".into(), 26.0)],
                refs: Vec::new(),
                strs: Vec::new(),
            }]));
        }
        let mut host = ScriptHost::new();
        host.set_scene_name("system");
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
        let warns: Vec<String> = host
            .drain_logs()
            .into_iter()
            .filter(|l| matches!(l.level, LogLevel::Warn))
            .map(|l| l.msg)
            .collect();
        assert_eq!(warns.len(), 1, "once per (script, param), not per instance: {warns:?}");
        let w = &warns[0];
        assert!(w.contains("laser_range"), "it names the param: {w}");
        assert!(w.contains("system"), "…the scene: {w}");
        assert!(w.contains("Belt"), "…the node: {w}");
        assert!(w.contains("tool"), "…and the script: {w}");
        assert!(!w.contains("reach"), "a param the script DOES declare is not reported: {w}");

        // Still silent on later passes — a warning per frame is a warning
        // nobody reads.
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        assert!(
            host.drain_logs().iter().all(|l| !matches!(l.level, LogLevel::Warn)),
            "reported once, not every frame"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The kind/tag index behind `findScript` (`floptle/0063`) has to answer
    /// exactly what the scan answered — FIRST IN SCENE ORDER — and it has to
    /// keep answering it after the scene changes. A stale index handing back a
    /// dead handle would be worse than the scan it replaced.
    #[test]
    fn the_script_index_answers_in_scene_order_and_follows_the_scene() {
        let dir = std::env::temp_dir().join(format!("floptle_0063_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "manager",
            "defaults = { id = 0 }\nfunction start(node)\n  myId = params.id\nend\n",
        );
        write_script(
            &dir,
            "probe",
            "\
function update(node, dt)
  local one = findScript('manager')
  log(string.format('%d %d %d', one and one.myId or -1,
                    #findScripts('manager'), #findTagged('crew')))
end
",
        );
        let (mut world, _driver) = world_with_script("probe");
        let mut managers = Vec::new();
        for i in 0..3 {
            let e = world.spawn();
            world.insert(e, Transform::IDENTITY);
            world.insert(e, floptle_core::Name(format!("m{i}")));
            world.insert(e, floptle_core::Tags(vec!["crew".into()]));
            world.insert(e, Scripts(vec![floptle_core::ScriptInst {
                kind: "manager".into(),
                enabled: true,
                params: vec![("id".into(), i as f32)],
                refs: Vec::new(),
                strs: Vec::new(),
            }]));
            managers.push(e);
        }
        let mut host = ScriptHost::new();
        // The script logs `first all tagged`; the last line is this pass's.
        let answer = |host: &mut ScriptHost, world: &mut World, t: f32| -> (i32, i32, i32) {
            host.run(world, &dir, 1.0 / 60.0, t);
            assert!(host.errors().is_empty(), "{:?}", host.errors());
            let last = host.drain_logs().pop().expect("the probe logged");
            let n: Vec<i32> = last.msg.split_whitespace().map(|s| s.parse().unwrap()).collect();
            (n[0], n[1], n[2])
        };
        // One pass to build every environment and seed its params; the reads
        // that matter come after.
        let _ = answer(&mut host, &mut world, 0.0);
        assert_eq!(
            answer(&mut host, &mut world, 1.0 / 60.0),
            (0, 3, 3),
            "the FIRST manager in scene order, and all three found"
        );

        // Despawn the first one: the index must follow, and the answer becomes
        // the next in order rather than a handle to something that is gone.
        // Despawn the first: the index must follow. WHICH survivor answers is
        // the ECS column's business (a despawn swaps the last row into the
        // hole, and the scan this replaced read the same order) — the
        // guarantee is that it is never the dead one.
        world.despawn(managers[0]);
        let (first, all, tagged) = answer(&mut host, &mut world, 2.0 / 60.0);
        assert!(first == 1 || first == 2, "a SURVIVING manager, not the despawned one: {first}");
        assert_eq!((all, tagged), (2, 2), "the index followed the despawn");

        // …and a script removed in the Inspector stops being found, while the
        // node itself (and its tag) stays.
        // A script removed in the Inspector stops being found; the node and
        // its tag stay.
        world.insert(managers[1], Scripts(Vec::new()));
        let (first, all, tagged) = answer(&mut host, &mut world, 3.0 / 60.0);
        assert_eq!((first, all, tagged), (2, 1, 2), "one manager script left, both tags stay");

        // Nothing at all: an empty answer, not a stale one.
        world.insert(managers[2], Scripts(Vec::new()));
        assert_eq!(
            answer(&mut host, &mut world, 4.0 / 60.0),
            (-1, 0, 2),
            "an empty answer, not a stale one"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `node:sprites()` on a node that is not a batch used to return a handle
    /// whose every draw was collected and then dropped by the renderer's own
    /// filter — no error, no warning, nothing drawn, ever (`floptle/0062`).
    #[test]
    fn asking_a_plain_node_for_a_sprite_batch_says_so() {
        let dir = std::env::temp_dir().join(format!("floptle_0062_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "flat",
            "function update(node, dt)\n  local b = node:sprites()\n  b:draw(1, 2)\nend\n",
        );
        let (mut world, e) = world_with_script("flat");
        world.insert(e, floptle_core::Matter::Empty);
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        let errs = host.errors().to_vec();
        assert_eq!(errs.len(), 1, "it must complain: {errs:?}");
        assert!(
            errs[0].contains("setSpriteBatch"),
            "…and name the call that fixes it: {}",
            errs[0]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A screen with a section switched off, written the way anybody writes it:
    /// `local dead = nil` and then the section in the list. That leaves a HOLE
    /// in the array, and a hole used to take the WHOLE SCREEN down — one absent
    /// section and nothing at all was built, with an error naming an index
    /// rather than a section. Found in a real project (`floptle/0061`).
    #[test]
    fn a_section_switched_off_does_not_take_the_screen_with_it() {
        let dir = std::env::temp_dir().join(format!("floptle_uimake_nil_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "hud",
            "\
function update(node, dt)
  local vitals = { 'text', text = 'HP' }
  local dead   = nil                      -- the section that is not on screen
  local hint   = { 'text', text = 'HINT' }
  ui.make(node, { vitals, dead, hint })
end
",
        );
        let (mut world, e) = world_with_script("hud");
        world.insert(e, floptle_core::Matter::Empty);
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
        host.apply_ui_makes(&mut world);
        assert_eq!(
            world.query::<floptle_core::Made>().count(),
            2,
            "the two sections that ARE on screen"
        );

        // …and the trailing half of the same problem: a hole truncates Lua's
        // length operator, so `hint` used to be dropped without a word.
        let texts: Vec<String> = world
            .query::<floptle_ui::ElementSpec>()
            .filter_map(|(_, s)| s.text.as_ref().map(|t| t.text.clone()))
            .collect();
        assert!(texts.contains(&"HINT".to_string()), "the section AFTER the hole: {texts:?}");

        // An empty table is how a screen is taken down. It used to describe one
        // anonymous box, so hiding a menu left an element behind every time.
        write_script(&dir, "hud", "function update(node, dt)\n  ui.make(node, {})\nend\n");
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        let destroy = host.apply_ui_makes(&mut world);
        assert_eq!(destroy.len(), 2, "both sections handed back for destruction");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `ui.make` raises on a property NAME it does not know, and the reasoning
    /// is right: a declarative screen that silently ignores a line is worse
    /// than one that stops. The same has to be true of a VALUE (`floptle/0072`).
    ///
    /// `pin = "topCenter"` used to answer `topLeft`, silently and forever. Four
    /// HUD elements — a floor readout, a controls hint, an interaction prompt
    /// and every shop note — stacked into one corner underneath the panel that
    /// legitimately lived there. The player's report was "the HUD is clipping
    /// over things and covering the scene", which is a perfect description of
    /// the symptom and points nowhere near the spelling that caused it.
    #[test]
    fn ui_make_refuses_a_value_a_property_does_not_take() {
        let dir = std::env::temp_dir().join(format!("floptle_uimake_val_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "hud",
            "function update(node, dt)\n  ui.make(node, { { 'text', text = 'X', pin = 'middle' } })\nend\n",
        );
        let (mut world, e) = world_with_script("hud");
        world.insert(e, floptle_core::Matter::Empty);
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        let err = host.errors().join(" ");
        assert!(!err.is_empty(), "a bad pin was accepted");
        // The message has to carry all three: which property, what it got, and
        // what it takes. Any one missing and you are back to re-reading a table.
        for want in ["pin", "middle", "topLeft", "bottomRight"] {
            assert!(err.contains(want), "the error never mentions {want}: {err}");
        }

        // …and the spelling people actually write is ANSWERED. This is the one
        // that was reported; refusing it would have been correct and useless.
        write_script(
            &dir,
            "hud",
            "function update(node, dt)\n  ui.make(node, { { 'text', text = 'X', pin = 'bottomCenter' } })\nend\n",
        );
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
        host.apply_ui_makes(&mut world);
        let pinned = world
            .query::<floptle_ui::ElementSpec>()
            .filter_map(|(_, s)| match s.place {
                floptle_ui::Place::Pin { anchor, .. } => Some(anchor),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            pinned.contains(&floptle_ui::Anchor::Bottom),
            "bottomCenter did not land at the bottom: {pinned:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Reconcile REUSES entities, so an element that was a buy button and is
    /// now a sold-out label is the same entity with no `clicked` in its new
    /// description. Its old closure used to stay armed — clicking one thing did
    /// another thing's job, intermittently, depending on what the screen last
    /// showed.
    #[test]
    fn an_element_that_stops_being_a_button_stops_answering_the_old_one() {
        let dir = std::env::temp_dir().join(format!("floptle_uimake_hook_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "shop",
            "\
function update(node, dt)
  ui.make(node, { { 'text', key = 'row', text = 'BUY',
                    button = true, onClicked = function() log('FIRED') end } })
end
",
        );
        let (mut world, e) = world_with_script("shop");
        world.insert(e, floptle_core::Matter::Empty);
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
        host.apply_ui_makes(&mut world);
        let row = world
            .query::<floptle_core::Made>()
            .find(|(_, m)| m.key == "row")
            .map(|(e, _)| e.index())
            .expect("the row");

        // It is a button, and it answers.
        let fired = |h: &mut ScriptHost| {
            h.drain_logs().iter().filter(|l| l.msg.contains("FIRED")).count()
        };
        let _ = fired(&mut host); // clear anything from the describe pass
        host.run_ui_hooks(&mut world, &[(row, "clicked")]);
        assert_eq!(fired(&mut host), 1, "the button works");

        // The same row, re-described as a plain label.
        write_script(
            &dir,
            "shop",
            "\
function update(node, dt)
  ui.make(node, { { 'text', key = 'row', text = 'SOLD OUT' } })
end
",
        );
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        host.apply_ui_makes(&mut world);
        let same = world
            .query::<floptle_core::Made>()
            .find(|(_, m)| m.key == "row")
            .map(|(e, _)| e.index());
        assert_eq!(same, Some(row), "reconcile kept the entity — that is the whole hazard");
        let _ = fired(&mut host);
        host.run_ui_hooks(&mut world, &[(row, "clicked")]);
        assert_eq!(fired(&mut host), 0, "…and it must NOT answer the old closure");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn captures_print_and_log() {
        let dir = std::env::temp_dir().join("floptle_script_test_logs");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "talky", "function update(node, dt)\n  log('tick')\n  print('p', 2, true)\nend\n");
        let (mut world, _e) = world_with_script("talky");
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        let logs = host.drain_logs();
        assert!(logs.iter().any(|l| l.msg == "tick" && l.level == LogLevel::Debug), "logs: {logs:?}");
        assert!(logs.iter().any(|l| l.msg == "p\t2\ttrue"), "logs: {logs:?}");
        // logs carry the originating script name for jump-to-source.
        assert!(logs.iter().any(|l| l.source.as_ref().is_some_and(|(n, _)| n == "talky")), "no source: {logs:?}");
        assert!(host.drain_logs().is_empty(), "logs should be drained");
    }

    #[test]
    fn captures_errors_in_console_feed() {
        let dir = std::env::temp_dir().join("floptle_script_test_err");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "broken", "function update(node, dt)\n  this_is_not_defined()\nend\n");
        let (mut world, _e) = world_with_script("broken");
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(!host.errors().is_empty(), "should report an error");
        let logs = host.drain_logs();
        assert!(logs.iter().any(|l| l.level == LogLevel::Error), "expected an error log: {logs:?}");
        assert!(logs.iter().any(|l| l.source.as_ref().is_some_and(|(n, _)| n == "broken")), "error lacks source: {logs:?}");
    }

    #[test]
    fn particles_api_queues_commands_and_reads_state() {
        let dir = std::env::temp_dir().join("floptle_script_test_vfx");
        let _ = std::fs::create_dir_all(&dir);
        // First frame: not playing → play(). Once the editor reports it playing, read
        // alive() into node.y.
        write_script(
            &dir,
            "smoke",
            "function update(node, dt)\n  local p = node:particles()\n  if p:isPlaying() then node.y = p:alive() else p:play() end\nend\n",
        );
        let (mut world, e) = world_with_script("smoke");
        world.insert(e, ParticleSystem { asset: "vfx/Smoke".into(), play_on_start: false });
        let mut host = ScriptHost::new();

        // Frame 1: empty info → isPlaying() false → the script queues play().
        host.run(&mut world, &dir, 0.1, 0.1);
        let cmds = host.take_vfx_commands();
        assert_eq!(cmds.len(), 1, "play() must queue exactly one command");
        assert!(matches!(cmds[0], (idx, VfxCmd::Play) if idx == e.index()), "wrong cmd: {cmds:?}");

        // Frame 2: the editor reports it playing with 12 alive → the script reads alive().
        host.set_vfx_info(HashMap::from([(
            e.index(),
            VfxInfo { playing: true, alive: 12, asset: "vfx/Smoke".into() },
        )]));
        host.run(&mut world, &dir, 0.1, 0.1);
        assert_eq!(
            world.get::<Transform>(e).unwrap().translation.y,
            12.0,
            "alive() must read the fed count"
        );
        assert!(host.take_vfx_commands().is_empty(), "no play() when already playing");
    }

    /// Ground truth for the `cond and X or Y` conditional idiom through the real
    /// host — with animator METHOD CALLS in the chain — plus the animator getters
    /// reading the fed mirror. Lua's ternary spelling is core syntax; the reported
    /// "errors writing statements like that" came from method casing (see
    /// `animator_method_typo_names_the_camel_case_fix`), not from the idiom.
    #[test]
    fn animator_getters_and_conditional_idiom() {
        let dir = std::env::temp_dir().join("floptle_script_test_anim");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "locomotion",
            "function update(node, dt)\n  local anim = node:animator()\n  node.x = anim:isPlaying('Running') and 2 or anim:isPlaying('Walking') and 1 or 0\n  node.y = anim:time() or -1\n  node.z = ((anim:current() == 'Running') and 10 or 0) + #anim:clips()\nend\n",
        );
        let (mut world, e) = world_with_script("locomotion");
        let mut host = ScriptHost::new();

        // The IDE's red-squiggle path must accept the idiom too.
        assert!(
            host.check_syntax(
                "function f(a) return a:isPlaying('R') and 2 or a:isPlaying('W') and 1 or 0 end"
            )
            .is_none(),
            "and/or chain must parse cleanly"
        );

        let info = |state: &str, t: f32, fin: bool| {
            HashMap::from([(
                e.index(),
                AnimInfo {
                    layers: vec![("Base".into(), Some(state.into()), t, fin)],
                    clips: Rc::new(
                        ["Idle", "Walking", "Running"]
                            .iter()
                            .map(|n| ClipInfo {
                                name: (*n).into(),
                                duration: 1.0,
                                events: Vec::new(),
                            })
                            .collect(),
                    ),
                },
            )])
        };
        let pos = |world: &World| world.get::<Transform>(e).unwrap().translation;

        // Running → the chain picks 2; current()/time()/clips() read the mirror.
        host.set_anim_info(info("Running", 0.25, false));
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let p = pos(&world);
        assert_eq!(p.x, 2.0, "isPlaying('Running') and 2 must win");
        assert!((p.y - 0.25).abs() < 1e-5, "time() reads the fed playhead");
        assert_eq!(p.z, 13.0, "current()=='Running' (10) + 3 clips");

        // Walking → the middle arm.
        host.set_anim_info(info("Walking", 1.5, false));
        host.run(&mut world, &dir, 0.1, 0.2);
        assert_eq!(pos(&world).x, 1.0, "isPlaying('Walking') and 1 must win");

        // Running but FINISHED → isPlaying is false → the chain falls to 0.
        host.set_anim_info(info("Running", 2.0, true));
        host.run(&mut world, &dir, 0.1, 0.3);
        assert_eq!(pos(&world).x, 0.0, "a finished one-shot is not 'playing'");

        // No animator mirror at all → every arm false → 0 (and no errors).
        host.set_anim_info(HashMap::new());
        host.run(&mut world, &dir, 0.1, 0.4);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(pos(&world).x, 0.0);
    }

    /// A CASING typo on an animator method (`anim:IsPlaying`) must fail with a
    /// did-you-mean naming the camelCase method — not a bare "attempt to call a
    /// nil value". Genuinely unknown keys still index to nil (feature probes).
    #[test]
    fn animator_method_typo_names_the_camel_case_fix() {
        let dir = std::env::temp_dir().join("floptle_script_test_anim_typo");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "typo",
            "function update(node, dt)\n  if dt < 0.15 then\n    if node:animator().notAThing == nil then node.y = 7 end\n  else\n    node.x = node:animator():IsPlaying('Run') and 2 or 0\n  end\nend\n",
        );
        let (mut world, e) = world_with_script("typo");
        let mut host = ScriptHost::new();
        // Frame 1: the unknown-key probe indexes to nil — no error, the write lands.
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.errors().is_empty(), "nil probe must not error: {:?}", host.errors());
        assert_eq!(world.get::<Transform>(e).unwrap().translation.y, 7.0);
        // Frame 2: the casing typo errors WITH the camelCase suggestion.
        host.run(&mut world, &dir, 0.2, 0.3);
        let errs = host.errors().join("\n");
        assert!(
            errs.contains("did you mean 'isPlaying'"),
            "typo must suggest the camelCase method: {errs}"
        );
    }

    #[test]
    fn audio_play_queues_and_handle_controls() {
        let dir = std::env::temp_dir().join("floptle_script_test_audio");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "sfx",
            "function update(node, dt)\n  local s = audio.play('audio/hit.ogg', 1.0, 2.0, 3.0, { maxDistance = 35, track = 'SFX', endBehavior = 'Destroy' })\n  s:setVolume(0.5)\n  audio.play('audio/music.ogg', { loop = true })\n  audio.track('Music'):setVolume(-6)\nend\n",
        );
        let (mut world, _e) = world_with_script("sfx");
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        let cmds = host.take_audio_commands();
        assert_eq!(cmds.len(), 4, "expected play+setVolume+play+trackVolume: {cmds:?}");
        let AudioCmd::Play { handle, clip, at, params } = &cmds[0] else {
            panic!("first cmd must be Play: {cmds:?}")
        };
        assert_eq!(clip, "audio/hit.ogg");
        assert!(matches!(at, AudioAt::Pos([1.0, 2.0, 3.0])), "positional play: {at:?}");
        assert_eq!(params.max_distance, 35.0);
        assert_eq!(params.track, "SFX");
        assert_eq!(params.end, floptle_audio::EndBehavior::Destroy);
        assert!(
            matches!(&cmds[1], AudioCmd::SetParam { handle: h, field, value }
                if h == handle && field == "volume" && *value == 0.5),
            "handle setter must target the played sound: {cmds:?}"
        );
        let AudioCmd::Play { at: at2, params: p2, .. } = &cmds[2] else {
            panic!("third cmd must be the flat play: {cmds:?}")
        };
        assert!(matches!(at2, AudioAt::Flat), "opts-only play is flat: {at2:?}");
        assert_eq!(p2.end, floptle_audio::EndBehavior::Loop, "loop = true shorthand");
        assert!(
            matches!(&cmds[3], AudioCmd::TrackVolume { track, db } if track == "Music" && *db == -6.0),
            "mixer track handle: {cmds:?}"
        );
        assert!(host.take_audio_commands().is_empty(), "drained");
    }

    #[test]
    fn node_sound_handle_and_component_mirror() {
        let dir = std::env::temp_dir().join("floptle_script_test_audio_src");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "alarm",
            "function update(node, dt)\n  if not node:sound():isPlaying() then node:sound():play() end\n  node:getcomponent('AudioSource').volume = 0.25\nend\n",
        );
        let (mut world, e) = world_with_script("alarm");
        world.insert(e, floptle_audio::AudioSource { clip: "audio/alarm.ogg".into(), ..Default::default() });
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        let cmds = host.take_audio_commands();
        assert!(
            matches!(cmds.as_slice(), [AudioCmd::SourcePlay { ent }] if *ent == e.index()),
            "not playing -> one SourcePlay: {cmds:?}"
        );
        assert_eq!(
            world.get::<floptle_audio::AudioSource>(e).unwrap().params.volume,
            0.25,
            "component mirror write must land on the ECS"
        );

        // Once the mirror says it's playing, no more play commands.
        let mut info = AudioInfo::default();
        info.sources.insert(
            e.index(),
            AudioPlayState { playing: true, paused: false, position: 0.5 },
        );
        host.set_audio_info(info);
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.take_audio_commands().is_empty(), "no play() when already playing");
    }

    #[test]
    fn spawn_effect_global_queues_a_one_shot() {
        let dir = std::env::temp_dir().join("floptle_script_test_spawnfx");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "boom",
            "function update(node, dt)\n  spawnEffect('vfx/Impact', 1.0, 2.0, 3.0)\nend\n",
        );
        let (mut world, _e) = world_with_script("boom");
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        let spawns = host.take_spawn_effects();
        assert_eq!(spawns.len(), 1, "one spawnEffect call = one queued one-shot");
        assert_eq!(spawns[0].0, "vfx/Impact");
        assert_eq!(spawns[0].1, [1.0, 2.0, 3.0]);
        assert!(host.take_spawn_effects().is_empty(), "drained");
    }

    #[test]
    fn getcomponent_toggles_particle_play_on_start() {
        let dir = std::env::temp_dir().join("floptle_script_test_vfx_comp");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "arm",
            "function update(node, dt)\n  node:getcomponent('ParticleSystem').play_on_start = 1\nend\n",
        );
        let (mut world, e) = world_with_script("arm");
        world.insert(e, ParticleSystem { asset: "vfx/Smoke".into(), play_on_start: false });
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(world.get::<ParticleSystem>(e).unwrap().play_on_start, "field must flush to the ECS");
    }

    #[test]
    fn input_api_drives_a_script() {
        let dir = std::env::temp_dir().join("floptle_script_test_input");
        let _ = std::fs::create_dir_all(&dir);
        // Move +z while "w" is held; jump (+y) on the click edge.
        write_script(
            &dir,
            "mover",
            "function update(node, dt)\n  if input.key('w') then node.z = node.z + 1.0 end\n  if input.clicked(0) then node.y = node.y + 5.0 end\nend\n",
        );
        let (mut world, e) = world_with_script("mover");
        let mut host = ScriptHost::new();

        // No input → no movement.
        host.run(&mut world, &dir, 0.1, 0.1);
        assert_eq!(world.get::<Transform>(e).unwrap().translation.z, 0.0);

        // Hold "w" + click → moves +z and jumps +y.
        let mut snap = InputSnapshot::default();
        snap.keys_down.insert("w".into());
        snap.buttons_pressed[0] = true;
        host.set_input(snap);
        host.run(&mut world, &dir, 0.1, 0.1);
        let t = world.get::<Transform>(e).unwrap();
        assert!(t.translation.z >= 1.0, "w should move +z, z={}", t.translation.z);
        assert!(t.translation.y >= 5.0, "click should jump +y, y={}", t.translation.y);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    }

    #[test]
    fn input_released_edge() {
        let dir = std::env::temp_dir().join("floptle_script_test_released");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "rel",
            "function update(node, dt)\n  if input.released('e') then node.x = node.x + 1 end\nend\n",
        );
        let (mut world, e) = world_with_script("rel");
        let mut host = ScriptHost::new();
        // Release edge → +1.
        let mut snap = InputSnapshot::default();
        snap.keys_released.insert("e".into());
        host.set_input(snap);
        host.run(&mut world, &dir, 0.1, 0.0);
        assert!((world.get::<Transform>(e).unwrap().translation.x - 1.0).abs() < 1e-6);
        // No release → unchanged.
        host.set_input(InputSnapshot::default());
        host.run(&mut world, &dir, 0.1, 0.0);
        assert!((world.get::<Transform>(e).unwrap().translation.x - 1.0).abs() < 1e-6);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    }

    #[test]
    fn node_hierarchy_traversal() {
        let dir = std::env::temp_dir().join("floptle_script_test_hier");
        let _ = std::fs::create_dir_all(&dir);
        // A child reads its parent's x (+1) and finds a sibling by name.
        write_script(
            &dir,
            "reader",
            "function update(node, dt)\n  local p = node.parent\n  if p then node.x = p.x + 1 end\nend\n",
        );
        let mut world = World::default();
        let parent = world.spawn();
        world.insert(
            parent,
            Transform::from_translation(floptle_core::math::DVec3::new(10.0, 0.0, 0.0)),
        );
        world.insert(parent, floptle_core::Name("Parent".into()));
        let child = world.spawn();
        world.insert(child, Transform::IDENTITY);
        world.insert(child, floptle_core::Parent(parent));
        world.insert(child, floptle_core::Name("Child".into()));
        world.insert(
            child,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "reader".into(),
                enabled: true,
                params: vec![], refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.016, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        // child.x = parent.x + 1 = 11 (local transforms, like the `node` argument).
        assert!(
            (world.get::<Transform>(child).unwrap().translation.x - 11.0).abs() < 1e-6,
            "child.x = {}",
            world.get::<Transform>(child).unwrap().translation.x
        );
    }

    /// `node.worldX/Y/Z` compose the parent chain: a unit under a moved,
    /// rotated, scaled container has to be able to answer "where am I, really?"
    /// — comparing a LOCAL x against a world-space order is how a click-to-move
    /// script walks off into the distance and never arrives.
    #[test]
    fn world_position_composes_the_parent_chain() {
        let dir = std::env::temp_dir().join("floptle_script_test_worldpos");
        let _ = std::fs::create_dir_all(&dir);
        // The script checks itself and raises on a mismatch — the host reports
        // a Lua error, which is what this test reads.
        write_script(
            &dir,
            "probe",
            "function update(node, dt)\n\
             \x20 local wx, wz = node.worldX, node.worldZ\n\
             \x20 if math.abs(wx - 10.0) > 1e-4 or math.abs(wz + 10.0) > 1e-4 then\n\
             \x20   error('world ' .. wx .. ',' .. wz)\n\
             \x20 end\n\
             \x20 if math.abs(node.x - 3.0) > 1e-6 then error('local x moved') end\n\
             end\n",
        );
        let mut world = World::default();
        let parent = world.spawn();
        let mut pt = Transform::from_translation(floptle_core::math::DVec3::new(10.0, 1.0, -4.0));
        pt.rotation = floptle_core::math::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        pt.scale = floptle_core::math::Vec3::splat(2.0);
        world.insert(parent, pt);
        let child = world.spawn();
        world.insert(
            child,
            Transform::from_translation(floptle_core::math::DVec3::new(3.0, 0.0, 0.0)),
        );
        world.insert(child, floptle_core::Parent(parent));
        world.insert(
            child,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "probe".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        // Parent local +X, scaled 2 and yawed 90°, lands 6 along −Z: (10, 1, −10).
        let want = floptle_core::world_transform(&world, child).translation;
        assert!((want.x - 10.0).abs() < 1e-6 && (want.z + 10.0).abs() < 1e-6, "{want:?}");
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.016, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    }

    /// A checkbox tunable reads as a real boolean inside a RUNNING script —
    /// not as the 1/0 it is stored as, and not as a truthy `0`.
    ///
    /// `env::params_table` has done this since the boolean round-trip fix, but
    /// only a unit test of that function said so, which is why three shipped
    /// examples still carried a private `on(v)` helper to defend against a
    /// problem the engine had already solved. This is the end-to-end statement
    /// that lets those helpers stay deleted: `if params.thing then` is correct
    /// on its own, from a script, with the box unticked.
    #[test]
    fn an_unticked_checkbox_param_is_false_inside_a_running_script() {
        let dir = std::env::temp_dir().join("floptle_script_test_checkbox");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "box",
            "defaults = { ring = true }\n\
             function update(node, dt)\n\
             \x20 if params.ring then node.x = 1 else node.x = -1 end\n\
             \x20 if type(params.ring) ~= 'boolean' then error('not a boolean: ' .. type(params.ring)) end\n\
             end\n",
        );
        for (stored, want_x) in [(0.0, -1.0), (1.0, 1.0)] {
            let mut world = World::default();
            let e = world.spawn();
            world.insert(e, Transform::IDENTITY);
            world.insert(
                e,
                Scripts(vec![floptle_core::ScriptInst {
                    kind: "box".into(),
                    enabled: true,
                    params: vec![("ring".into(), stored)],
                    refs: Vec::new(),
                    strs: Vec::new(),
                }]),
            );
            let mut host = ScriptHost::new();
            host.run(&mut world, &dir, 0.016, 0.0);
            assert!(host.errors().is_empty(), "stored={stored}: {:?}", host.errors());
            assert_eq!(
                world.get::<Transform>(e).unwrap().translation.x,
                want_x,
                "a stored {stored} must read as {}",
                stored != 0.0
            );
        }
    }

    /// The local ↔ world set, against the frame that breaks a naive
    /// implementation: a parent that is moved, ROTATED and SCALED.
    ///
    /// `node:setWorldPos` and `node:moveTowards` go back through
    /// `Transform::inv_mul` rather than decomposing a matrix — the componentwise
    /// TRS inverse, whose doc comment explains why the matrix route puts a
    /// mirrored parent's negative determinant on the wrong axis. The mirrored
    /// case is the last assertion here, and it is the one that would silently
    /// pass with the wrong maths on an un-mirrored parent.
    #[test]
    fn local_and_world_conversions_survive_a_rotated_scaled_parent() {
        let dir = std::env::temp_dir().join("floptle_script_test_localworld");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "probe",
            "function update(node, dt)\n\
             \x20 local function near(a, b, what)\n\
             \x20   if math.abs(a - b) > 1e-4 then error(what .. ': ' .. a .. ' ~= ' .. b) end\n\
             \x20 end\n\
             \x20 -- toWorld/toLocal round trip through the whole chain.\n\
             \x20 local back = node:toLocal(node:toWorld(vec3(1, 2, 3)))\n\
             \x20 near(back.x, 1, 'toLocal x') near(back.y, 2, 'toLocal y') near(back.z, 3, 'toLocal z')\n\
             \x20 -- The node's own origin in world space is node.worldPos.\n\
             \x20 local o = node:toWorld(vec3(0, 0, 0))\n\
             \x20 near(o.x, node.worldX, 'origin x') near(o.z, node.worldZ, 'origin z')\n\
             \x20 -- worldForward composes the parent's rotation; node.forward does not.\n\
             \x20 near(node:worldForward():length(), 1, 'forward is unit')\n\
             \x20 near(node:worldForward().x, -1, 'parent yaw 90 turns -Z into -X')\n\
             \x20 -- setWorldPos: ask for a world point, land on it.\n\
             \x20 node:setWorldPos(vec3(2, 7, -3))\n\
             \x20 near(node.worldX, 2, 'set x') near(node.worldY, 7, 'set y') near(node.worldZ, -3, 'set z')\n\
             \x20 -- distanceTo/Flat are WORLD measurements.\n\
             \x20 near(node:distanceTo(vec3(2, 7, -3)), 0, 'distanceTo self')\n\
             \x20 near(node:distanceFlat(vec3(2, 99, -3)), 0, 'distanceFlat ignores up')\n\
             \x20 near(node:distanceTo(vec3(2, 99, -3)), 92, 'distanceTo does not')\n\
             \x20 -- moveTowards steps in world space and never overshoots.\n\
             \x20 node:moveTowards(vec3(2, 7, 7), 4)\n\
             \x20 near(node.worldZ, 1, 'moveTowards stepped 4 of 10')\n\
             \x20 local arrived = node:moveTowards(vec3(2, 7, 7), 999)\n\
             \x20 near(node.worldZ, 7, 'moveTowards landed exactly')\n\
             \x20 if not arrived then error('moveTowards should report arrival') end\n\
             end\n",
        );
        // Two runs: an ordinary parent, then a MIRRORED one (negative Y scale).
        for mirror in [false, true] {
            let mut world = World::default();
            let parent = world.spawn();
            let mut pt =
                Transform::from_translation(floptle_core::math::DVec3::new(10.0, 1.0, -4.0));
            pt.rotation = floptle_core::math::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
            pt.scale = if mirror {
                floptle_core::math::Vec3::new(2.0, -1.5, 2.0)
            } else {
                floptle_core::math::Vec3::splat(2.0)
            };
            world.insert(parent, pt);
            let child = world.spawn();
            world.insert(
                child,
                Transform::from_translation(floptle_core::math::DVec3::new(3.0, 0.5, 0.0)),
            );
            world.insert(child, floptle_core::Parent(parent));
            world.insert(
                child,
                Scripts(vec![floptle_core::ScriptInst {
                    kind: "probe".into(),
                    enabled: true,
                    params: vec![],
                    refs: Vec::new(),
                    strs: Vec::new(),
                }]),
            );
            let mut host = ScriptHost::new();
            host.run(&mut world, &dir, 0.016, 0.0);
            assert!(
                host.errors().is_empty(),
                "mirror={mirror} errors: {:?}",
                host.errors()
            );
        }
    }

    /// `node:lookAt` and `node:turnTowards` — the two names that replace an
    /// `atan2` with two minus signs and a shortest-arc dance across the ±π seam.
    #[test]
    fn look_at_faces_the_target_and_turn_towards_takes_the_short_way() {
        let dir = std::env::temp_dir().join("floptle_script_test_lookat");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "aim",
            "function update(node, dt)\n\
             \x20 local function near(a, b, what)\n\
             \x20   if math.abs(a - b) > 1e-4 then error(what .. ': ' .. a .. ' ~= ' .. b) end\n\
             \x20 end\n\
             \x20 -- Straight down -Z is yaw 0; the target is a plain world point.\n\
             \x20 node:lookAt(vec3(0, 0, -10))\n\
             \x20 near(node.yaw, 0, 'yaw at a -Z target')\n\
             \x20 near(node.pitch, 0, 'pitch at a level target')\n\
             \x20 -- A NODE handle aims at where that node WORLD is.\n\
             \x20 node:lookAt(find('Target'))\n\
             \x20 near(node.yaw, math.pi / 2, 'yaw at a -X target')\n\
             \x20 -- Looking up: +Y target, pitch positive.\n\
             \x20 node:lookAt(vec3(0, 10, -10))\n\
             \x20 near(node.pitch, math.pi / 4, 'pitch at a raised target')\n\
             \x20 -- turnTowards steps by at most maxRadians, the SHORT way across\n\
             \x20 -- the seam: from -170 deg toward +170 deg is +20, not -340.\n\
             \x20 node.yaw = math.rad(-170)\n\
             \x20 node.pitch = 0\n\
             \x20 node:turnTowards(dirFromYaw(math.rad(170)), math.rad(5))\n\
             \x20 near(math.deg(node.yaw), -175, 'turnTowards went the long way')\n\
             \x20 -- A big enough step lands exactly on the target angle.\n\
             \x20 node:turnTowards(dirFromYaw(math.rad(170)), math.pi)\n\
             \x20 near(math.abs(math.deg(node.yaw)), 170, 'turnTowards should arrive')\n\
             \x20 -- A zero direction leaves the facing alone (no NaN, no snap).\n\
             \x20 local was = node.yaw\n\
             \x20 node:turnTowards(vec3(0, 0, 0), 1)\n\
             \x20 near(node.yaw, was, 'a zero direction must not move the facing')\n\
             end\n",
        );
        let mut world = World::default();
        let target = world.spawn();
        world.insert(
            target,
            Transform::from_translation(floptle_core::math::DVec3::new(-10.0, 0.0, 0.0)),
        );
        world.insert(target, floptle_core::Name("Target".into()));
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "aim".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.016, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    }

    #[test]
    fn cross_script_reference_method_and_state() {
        let dir = std::env::temp_dir().join("floptle_script_test_xref");
        let _ = std::fs::create_dir_all(&dir);
        // A manager holds state + a method; the method moves its own node via `node`.
        write_script(
            &dir,
            "manager",
            "score = 0\nfunction addScore(n)\n  score = score + n\n  node.x = score\nend\nfunction update(node, dt) end\n",
        );
        // A ticker finds the manager anywhere in the scene and calls its method.
        write_script(
            &dir,
            "ticker",
            "function update(node, dt)\n  local m = findScript('manager')\n  if m then m.addScore(5) end\nend\n",
        );
        let mut world = World::default();
        let mgr = world.spawn();
        world.insert(mgr, Transform::IDENTITY);
        world.insert(mgr, floptle_core::Name("Manager".into()));
        world.insert(
            mgr,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "manager".into(),
                enabled: true,
                params: vec![], refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let t = world.spawn();
        world.insert(t, Transform::IDENTITY);
        world.insert(
            t,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "ticker".into(),
                enabled: true,
                params: vec![], refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        for _ in 0..3 {
            host.run(&mut world, &dir, 0.016, 0.0);
        }
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        // 3 frames × +5 = 15; the manager moved itself to x = score via its node handle.
        assert!(
            (world.get::<Transform>(mgr).unwrap().translation.x - 15.0).abs() < 1e-6,
            "manager.x = {}",
            world.get::<Transform>(mgr).unwrap().translation.x
        );
    }

    #[test]
    fn script_reads_and_swaps_mesh_model() {
        // node.model reflects the current Mesh asset; assigning it swaps the model
        // (applied to the ECS in run + reported via take_model_changes for re-import).
        let dir = std::env::temp_dir().join("floptle_script_test_model");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "swap",
            "function update(node, dt)\n  if node.model == \"assets/models/old.glb\" then node.model = \"assets/models/new.glb\" end\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Matter::Mesh { asset_path: "assets/models/old.glb".into() });
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "swap".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        match world.get::<Matter>(e).unwrap() {
            Matter::Mesh { asset_path } => assert_eq!(asset_path, "assets/models/new.glb"),
            other => panic!("expected mesh, got {other:?}"),
        }
        let changes = host.take_model_changes();
        assert_eq!(changes.get(&e.index()).map(|s| s.as_str()), Some("assets/models/new.glb"));
    }

    #[test]
    fn noderef_param_resolves_to_a_handle_and_rebinds_by_name() {
        // defaults = { target = noderef() } + an Inspector-wired name -> the script
        // sees a node handle in params (no find()); unwired refs read nil.
        let dir = std::env::temp_dir().join("floptle_script_test_noderef");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "aimer",
            concat!(
                "defaults = { target = noderef(), missing = noderef(), speed = 2 }\n",
                "function update(node, dt)\n",
                "  if params.target then params.target.y = 5 end\n",
                "  node.x = (params.missing == nil and 1 or 0) + params.speed\n",
                "end\n",
            ),
        );
        let mut world = World::default();
        let driver = world.spawn();
        world.insert(driver, Transform::IDENTITY);
        world.insert(
            driver,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "aimer".into(),
                enabled: true,
                params: vec![],
                refs: vec![
                    ("target".into(), "Turret".into()),
                    ("missing".into(), String::new()),
                ],
                strs: Vec::new(),
            }]),
        );
        let turret = world.spawn();
        world.insert(turret, Transform::IDENTITY);
        world.insert(turret, floptle_core::Name("Turret".into()));
        let mut host = ScriptHost::new();
        // The defaults surface reports the ref params for the Inspector.
        let path = dir.join("aimer.lua");
        let (nums, refs, _strs) = host.script_defaults(&path);
        assert_eq!(
            refs,
            vec![
                ("missing".to_string(), RefKind::Node),
                ("target".to_string(), RefKind::Node)
            ]
        );
        assert_eq!(nums, vec![("speed".to_string(), 2.0)]);
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(world.get::<Transform>(turret).unwrap().translation.y, 5.0);
        // missing == nil (1) + speed (2): the sentinel never leaks as a string.
        assert_eq!(world.get::<Transform>(driver).unwrap().translation.x, 3.0);
    }

    #[test]
    fn scriptref_and_componentref_bind_handles_directly() {
        // scriptref("health") gives the wired node's health SCRIPT handle;
        // componentref("RigidBody") gives its component handle; a wire to a node
        // MISSING the declared thing reads nil (validated, not a dead handle).
        let dir = std::env::temp_dir().join("floptle_script_test_kindrefs");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "health", "hp = 40\nfunction damage(n)\n  hp = hp - n\nend\n");
        write_script(
            &dir,
            "attacker",
            concat!(
                "defaults = { victim = scriptref(\"health\"), body = componentref(\"RigidBody\"),\n",
                "             bogus = componentref(\"PointLight\") }\n",
                "function update(node, dt)\n",
                "  if params.victim then params.victim.damage(15) end\n",
                "  if params.body then params.body.friction = 0.05 end\n",
                "  node.x = (params.bogus == nil) and 1 or 0\n",
                "end\n",
            ),
        );
        let mut world = World::default();
        let attacker = world.spawn();
        world.insert(attacker, Transform::IDENTITY);
        world.insert(
            attacker,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "attacker".into(),
                enabled: true,
                params: vec![],
                refs: vec![
                    ("victim".into(), "Dummy".into()),
                    ("body".into(), "Dummy".into()),
                    ("bogus".into(), "Dummy".into()), // Dummy has no PointLight → nil
                ],
                strs: Vec::new(),
            }]),
        );
        let dummy = world.spawn();
        world.insert(dummy, Transform::IDENTITY);
        world.insert(dummy, floptle_core::Name("Dummy".into()));
        world.insert(dummy, RigidBody::default());
        world.insert(
            dummy,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "health".into(),
                enabled: true,
                params: vec![],
                refs: vec![],
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        // The health script's state took the damage call.
        let hp: f64 = host.instance_env(dummy.index(), "health").unwrap().get("hp").unwrap();
        assert_eq!(hp, 25.0);
        assert_eq!(world.get::<RigidBody>(dummy).unwrap().friction, 0.05);
        assert_eq!(world.get::<Transform>(attacker).unwrap().translation.x, 1.0);
    }

    #[test]
    fn ui_hook_events_reach_the_node_scripts() {
        // A clicked/hoverStart event fires the same-named function on the node's
        // scripts, with a node handle argument; writes flush like any handle write.
        let dir = std::env::temp_dir().join("floptle_script_test_ui_hooks");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "btn",
            concat!(
                "function clicked(node)\n  node.y = node.y + 1\n",
                "  local c = node:getcomponent(\"UiElement\")\n",
                "  if c then c.opacity = 0.25 end\nend\n",
                "function hoverStart(node)\n  node.z = 7\nend\n",
            ),
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, floptle_core::Name("Play".into()));
        world.insert(
            e,
            floptle_ui::ElementSpec { button: true, ..Default::default() },
        );
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "btn".into(),
                enabled: true,
                params: vec![],
                refs: vec![],
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0); // builds the instance envs
        host.run_ui_hooks(&mut world, &[(e.index(), "hoverStart"), (e.index(), "clicked")]);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let tr = world.get::<Transform>(e).unwrap();
        assert_eq!((tr.translation.y, tr.translation.z), (1.0, 7.0));
        assert_eq!(world.get::<floptle_ui::ElementSpec>(e).unwrap().opacity, 0.25);
    }

    /// Build a world with a menu node (carrying `script`) and `n` buttons named
    /// `Btn1`…`Btn<n>`, plus one plain box named `Scenery`.
    fn menu_world(script: &str, n: usize) -> (World, floptle_core::Entity, Vec<u32>) {
        let mut world = World::default();
        let menu = world.spawn();
        world.insert(menu, Transform::IDENTITY);
        world.insert(menu, floptle_core::Name("Menu".into()));
        world.insert(
            menu,
            Scripts(vec![floptle_core::ScriptInst {
                kind: script.into(),
                enabled: true,
                params: vec![],
                refs: vec![],
                strs: Vec::new(),
            }]),
        );
        let mut ids = Vec::new();
        for i in 1..=n {
            let b = world.spawn();
            world.insert(b, Transform::IDENTITY);
            world.insert(b, floptle_core::Name(format!("Btn{i}")));
            world.insert(b, floptle_ui::ElementSpec { button: true, ..Default::default() });
            ids.push(b.index());
        }
        let scenery = world.spawn();
        world.insert(scenery, Transform::IDENTITY);
        world.insert(scenery, floptle_core::Name("Scenery".into()));
        world.insert(scenery, floptle_ui::ElementSpec::default());
        (world, menu, ids)
    }

    /// `ui.on(element, hook, fn)`: one manager script answers for buttons it
    /// does not live on — the point of the whole thing, since the alternative
    /// is a script file per button.
    ///
    /// Also pins the two properties that make it safe to write: registering
    /// again REPLACES (so calling it from `update` costs one closure, not one
    /// per frame), and `ui.off` stops it.
    #[test]
    fn a_manager_hears_buttons_it_does_not_live_on() {
        let dir = std::env::temp_dir().join("floptle_script_test_ui_on");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "menu",
            concat!(
                "hits = 0\n",
                "last = \"\"\n",
                "lastEvent = \"\"\n",
                // Registered from `update`, deliberately: re-registering the
                // same (element, hook) must replace rather than stack.
                "function update(node, dt)\n",
                "  for i = 1, 2 do\n",
                "    ui.on(find(\"Btn\" .. i), \"clicked\", function(el, ev)\n",
                "      hits = hits + 1\n",
                "      last = el.name\n",
                "      lastEvent = ev\n",
                "    end)\n",
                "  end\n",
                "end\n",
                "function stopListening(node)\n  ui.off(find(\"Btn1\"))\nend\n",
            ),
        );
        let (mut world, menu, btns) = menu_world("menu", 2);
        let mut host = ScriptHost::new();
        for _ in 0..3 {
            host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        }
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let read = |host: &ScriptHost, key: &str| -> String {
            host.instance_env(menu.index(), "menu")
                .and_then(|e| e.get::<String>(key).ok())
                .unwrap_or_default()
        };
        let hits = |host: &ScriptHost| -> f64 {
            host.instance_env(menu.index(), "menu")
                .and_then(|e| e.get::<f64>("hits").ok())
                .unwrap_or_default()
        };

        host.run_ui_hooks(&mut world, &[(btns[1], "clicked")]);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(hits(&host), 1.0, "three frames of ui.on must leave ONE listener");
        assert_eq!(read(&host, "last"), "Btn2", "the element that fired is the argument");
        assert_eq!(read(&host, "lastEvent"), "clicked", "…and the hook name rides along");

        // A hook the manager never asked for reaches nothing.
        host.run_ui_hooks(&mut world, &[(btns[0], "hoverStart")]);
        assert_eq!(hits(&host), 1.0);
        host.run_ui_hooks(&mut world, &[(btns[0], "clicked")]);
        assert_eq!(hits(&host), 2.0);

        // `ui.off` — and only for the element named.
        host.call_action(&mut world, &dir, menu.index(), "menu", "stopListening");
        host.run_ui_hooks(&mut world, &[(btns[0], "clicked"), (btns[1], "clicked")]);
        assert_eq!(hits(&host), 3.0, "Btn1 is off, Btn2 still listening");
        assert_eq!(read(&host, "last"), "Btn2");
    }

    /// A listener dies with the script that registered it. Destroying a menu
    /// manager must not leave its closures answering buttons — the closure also
    /// holds that script's whole environment alive.
    #[test]
    fn a_listener_dies_with_the_script_that_registered_it() {
        let dir = std::env::temp_dir().join("floptle_script_test_ui_on_life");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "mgr",
            concat!(
                "hits = 0\n",
                "function update(node, dt)\n",
                "  ui.on(find(\"Btn1\"), \"clicked\", function() hits = hits + 1 end)\n",
                "end\n",
            ),
        );
        let (mut world, menu, btns) = menu_world("mgr", 1);
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        host.run_ui_hooks(&mut world, &[(btns[0], "clicked")]);
        let hits = |host: &ScriptHost| -> f64 {
            host.instance_env(menu.index(), "mgr")
                .and_then(|e| e.get::<f64>("hits").ok())
                .unwrap_or_default()
        };
        assert_eq!(hits(&host), 1.0);
        // The manager goes away (the driver reports what it destroyed).
        host.drop_ui_handlers(&[menu.index()]);
        host.run_ui_hooks(&mut world, &[(btns[0], "clicked")]);
        assert_eq!(hits(&host), 1.0, "a destroyed manager stops answering");
        // …and so does a listener whose ELEMENT went away — entity indices are
        // reused, so a stale one would fire on whatever inherits the slot.
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0); // update re-registers
        host.drop_ui_handlers(&[btns[0]]);
        host.run_ui_hooks(&mut world, &[(btns[0], "clicked")]);
        assert_eq!(hits(&host), 1.0, "the element is gone, so nothing fires for it");
    }

    /// The other half: a script that would rather ask than be called back.
    /// `ui.clicked(el)` / `ui.events()` read the SAME list the hooks fire from,
    /// published before the run — so a poll and a hook can't disagree.
    #[test]
    fn this_frames_ui_events_can_be_polled() {
        let dir = std::env::temp_dir().join("floptle_script_test_ui_poll");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "poll",
            concat!(
                "clicks = \"\"\n",
                "seen = 0\n",
                "hover = \"\"\n",
                "function update(node, dt)\n",
                "  if ui.clicked(find(\"Btn1\")) then clicks = clicks .. \"1\" end\n",
                "  if ui.clicked(find(\"Btn2\")) then clicks = clicks .. \"2\" end\n",
                "  seen = #ui.events(\"clicked\")\n",
                "  local h = ui.hovered()\n",
                "  hover = h and h.name or \"\"\n",
                "  if ui.hovered(find(\"Btn2\")) then hover = hover .. \"!\" end\n",
                "end\n",
            ),
        );
        let (mut world, menu, btns) = menu_world("poll", 2);
        let mut host = ScriptHost::new();
        host.set_ui_frame_state(&[(btns[1], "clicked"), (btns[0], "hoverStart")], Some(btns[1]), None);
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let env = host.instance_env(menu.index(), "poll").expect("live instance");
        assert_eq!(env.get::<String>("clicks").unwrap(), "2");
        assert_eq!(env.get::<f64>("seen").unwrap(), 1.0, "hoverStart is not a click");
        assert_eq!(env.get::<String>("hover").unwrap(), "Btn2!");

        // Next frame, nothing happened: the answers are per-frame, not sticky.
        host.set_ui_frame_state(&[], None, None);
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        let env = host.instance_env(menu.index(), "poll").expect("live instance");
        assert_eq!(env.get::<String>("clicks").unwrap(), "2", "no new clicks");
        assert_eq!(env.get::<f64>("seen").unwrap(), 0.0);
        assert_eq!(env.get::<String>("hover").unwrap(), "");
    }

    /// Listening for a click on something that takes no clicks is the one
    /// mistake this API makes easy, and it leaves NOTHING to look at. It warns.
    #[test]
    fn listening_to_an_element_that_takes_no_clicks_warns() {
        let dir = std::env::temp_dir().join("floptle_script_test_ui_on_warn");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "oops",
            concat!(
                "function start(node)\n",
                "  ui.on(find(\"Scenery\"), \"clicked\", function() end)\n",
                "  ui.on(find(\"Btn1\"), \"clicked\", function() end)\n",
                "end\n",
            ),
        );
        let (mut world, _menu, _btns) = menu_world("oops", 1);
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "a warning, not an error: {:?}", host.errors());
        let warnings: Vec<String> = host
            .drain_logs()
            .into_iter()
            .filter(|l| l.level == LogLevel::Warn)
            .map(|l| l.msg)
            .collect();
        assert_eq!(warnings.len(), 1, "only the wrong one warns: {warnings:?}");
        assert!(warnings[0].contains("Scenery"), "{}", warnings[0]);
        assert!(warnings[0].contains("Button"), "it names the fix: {}", warnings[0]);
        // Warned once, at registration — not every frame it fails to fire.
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        assert!(host.drain_logs().iter().all(|l| l.level != LogLevel::Warn), "warned once");
    }

    /// A mistyped hook is the other silent failure — `ui.on(b, "onClicked", …)`
    /// would register a listener nothing ever calls. It raises instead.
    #[test]
    fn a_mistyped_hook_name_raises() {
        let dir = std::env::temp_dir().join("floptle_script_test_ui_on_typo");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "typo",
            "function start(node)\n  ui.on(find(\"Btn1\"), \"onClicked\", function() end)\nend\n",
        );
        let (mut world, _menu, _btns) = menu_world("typo", 1);
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        let errs = format!("{:?}", host.errors());
        assert!(errs.contains("not a UI hook"), "{errs}");
        assert!(errs.contains("clicked"), "it lists the real ones: {errs}");
    }

    /// `ui.make` end to end: a Lua table becomes real nodes, a described
    /// button's inline `onClicked` fires on a click, and re-describing the
    /// screen with one fewer row destroys exactly that row.
    ///
    /// The pieces have their own tests; this one is the whole path, because
    /// that is where the seams are — the parse hands paths to the reconcile,
    /// the reconcile hands entities back, and the closures have to end up on
    /// the right ones.
    #[test]
    fn ui_make_builds_a_screen_and_its_buttons_work() {
        let dir = std::env::temp_dir().join("floptle_script_test_ui_make");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "screen",
            concat!(
                "crew = { \"ana\", \"bo\", \"cy\" }\n",
                "picked = \"\"\n",
                "function build(node)\n",
                "  ui.make(node, { \"col\", gap = 6, items = crew,\n",
                "    function(id) return { \"button\", key = id, text = id,\n",
                "      onClicked = function(n) picked = id end } end })\n",
                "end\n",
                "function start(node) build(node) end\n",
            ),
        );
        let mut world = World::default();
        let panel = world.spawn();
        world.insert(panel, Transform::IDENTITY);
        world.insert(panel, floptle_core::Name("Panel".into()));
        world.insert(panel, floptle_ui::ElementSpec::default());
        world.insert(
            panel,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "screen".into(),
                enabled: true,
                params: vec![],
                refs: vec![],
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.apply_ui_makes(&mut world).is_empty(), "nothing to destroy on the first build");
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

        // A column under the panel, three buttons under that.
        let made: Vec<floptle_core::Entity> =
            world.query::<floptle_core::Made>().map(|(e, _)| e).collect();
        assert_eq!(made.len(), 4, "one column + three rows");
        let mut rows: Vec<(u32, floptle_core::Entity)> = world
            .query::<floptle_core::Made>()
            .filter(|(_, m)| m.kind == "button")
            .map(|(e, m)| (m.slot, e))
            .collect();
        rows.sort_by_key(|(slot, e)| (*slot, e.index()));
        assert_eq!(rows.len(), 3);
        let texts: Vec<String> = rows
            .iter()
            .map(|(_, e)| {
                world.get::<floptle_ui::ElementSpec>(*e).unwrap().text.as_ref().unwrap().text.clone()
            })
            .collect();
        assert_eq!(texts, vec!["ana", "bo", "cy"]);

        // The middle row's inline closure runs on a click, with no script
        // file, no prefab and no `clicked` function anywhere.
        host.run_ui_hooks(&mut world, &[(rows[1].1.index(), "clicked")]);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let picked: String = host
            .instance_env(panel.index(), "screen")
            .and_then(|env| env.get::<String>("picked").ok())
            .unwrap_or_default();
        assert_eq!(picked, "bo");

        // Describing the same screen again changes nothing…
        host.call_action(&mut world, &dir, panel.index(), "screen", "build");
        assert!(host.apply_ui_makes(&mut world).is_empty(), "a re-render must not churn");
        assert_eq!(world.query::<floptle_core::Made>().count(), 4);

        // …and dropping a row hands back exactly that row.
        let env = host.instance_env(panel.index(), "screen").expect("the instance is live");
        env.set("crew", vec!["ana".to_string(), "cy".to_string()]).unwrap();
        host.call_action(&mut world, &dir, panel.index(), "screen", "build");
        assert_eq!(host.apply_ui_makes(&mut world), vec![rows[1].1.index()]);
    }

    /// `node:setShaderParam` lands in the UI element's `shader_params` when it
    /// carries a `stage ui` shader, and in the Material's otherwise — the
    /// bridge instruments (navball) drive their uniforms through.
    #[test]
    fn set_shader_param_reaches_element_and_material() {
        let dir = std::env::temp_dir().join("floptle_script_test_shader_param");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "inst",
            concat!(
                "function update(node, dt)\n",
                "  node:setShaderParam(\"nose\", 0.1, 0.9, 0.2)\n",
                "  local m = find(\"Meshy\")\n",
                "  m:setShaderParam(\"glow\", 2.5)\n",
                "end\n",
            ),
        );
        let mut world = World::default();
        let ball = world.spawn();
        world.insert(ball, Transform::IDENTITY);
        world.insert(ball, floptle_core::Name("Ball".into()));
        world.insert(
            ball,
            floptle_ui::ElementSpec { shader: "shaders/navball.flsl".into(), ..Default::default() },
        );
        world.insert(
            ball,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "inst".into(),
                enabled: true,
                params: vec![],
                refs: vec![],
                strs: Vec::new(),
            }]),
        );
        let meshy = world.spawn();
        world.insert(meshy, Transform::IDENTITY);
        world.insert(meshy, floptle_core::Name("Meshy".into()));
        world.insert(meshy, Material { shader: Some("shaders/x.flsl".into()), ..Default::default() });
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let spec = world.get::<floptle_ui::ElementSpec>(ball).unwrap();
        assert_eq!(spec.shader_params.get("nose"), Some(&[0.1, 0.9, 0.2, 0.0]));
        let mat = world.get::<Material>(meshy).unwrap();
        assert_eq!(mat.shader_params.get("glow"), Some(&[2.5, 0.0, 0.0, 0.0]));
    }

    #[test]
    fn script_drives_ui_text_slider_and_element_fields() {
        // The HUD path: node.text swaps a label, getcomponent("UiSlider").value
        // drives a health bar, getcomponent("UiElement") reaches visibility etc.
        let dir = std::env::temp_dir().join("floptle_script_test_ui");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "hud",
            concat!(
                "function update(node, dt)\n",
                "  local label = find(\"HpLabel\")\n",
                "  label.text = 42\n",
                "  local bar = find(\"HpBar\")\n",
                "  bar:getcomponent(\"UiSlider\").value = 25\n",
                "  bar:getcomponent(\"UiElement\").opacity = 0.5\n",
                "  node.x = (label.text == \"42\" and 1 or 0)\n",
                "end\n",
            ),
        );
        let mut world = World::default();
        let driver = world.spawn();
        world.insert(driver, Transform::IDENTITY);
        world.insert(
            driver,
            Scripts(vec![floptle_core::ScriptInst { kind: "hud".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
        );
        let label = world.spawn();
        world.insert(label, Transform::IDENTITY);
        world.insert(label, floptle_core::Name("HpLabel".into()));
        world.insert(
            label,
            floptle_ui::ElementSpec {
                text: Some(floptle_ui::TextSpec { text: "hp".into(), ..Default::default() }),
                ..Default::default()
            },
        );
        let bar = world.spawn();
        world.insert(bar, Transform::IDENTITY);
        world.insert(bar, floptle_core::Name("HpBar".into()));
        world.insert(
            bar,
            floptle_ui::ElementSpec {
                slider: Some(floptle_ui::SliderSpec::default()),
                ..Default::default()
            },
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let lspec = world.get::<floptle_ui::ElementSpec>(label).unwrap();
        assert_eq!(lspec.text.as_ref().unwrap().text, "42");
        let bspec = world.get::<floptle_ui::ElementSpec>(bar).unwrap();
        assert_eq!(bspec.slider.unwrap().value, 25.0);
        assert_eq!(bspec.opacity, 0.5);
        // Read-your-writes: the script saw its own label.text assignment.
        assert_eq!(world.get::<Transform>(driver).unwrap().translation.x, 1.0);
    }

    /// `node.style`, `disabled` and `selected` — the state channel a menu
    /// drives. Read-your-writes matters here as much as it does for `text`:
    /// a row script routinely sets a style and then reads it back to decide
    /// what else to do.
    #[test]
    fn script_drives_ui_style_and_states() {
        let dir = std::env::temp_dir().join("floptle_script_test_ui_style");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "row",
            concat!(
                "function update(node, dt)\n",
                "  local r = find(\"Row\")\n",
                "  r.style = \"button/danger\"\n",
                "  local e = r:getcomponent(\"UiElement\")\n",
                "  e.selected = 1\n",
                "  e.disabled = 0\n",
                "  node.x = (r.style == \"button/danger\") and 1 or 0\n",
                "end\n",
            ),
        );
        let mut world = World::default();
        let driver = world.spawn();
        world.insert(driver, Transform::IDENTITY);
        world.insert(
            driver,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "row".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let row = world.spawn();
        world.insert(row, Transform::IDENTITY);
        world.insert(row, floptle_core::Name("Row".into()));
        world.insert(
            row,
            floptle_ui::ElementSpec { style: "row".into(), disabled: true, ..Default::default() },
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let spec = world.get::<floptle_ui::ElementSpec>(row).unwrap();
        assert_eq!(spec.style, "button/danger");
        assert!(spec.selected);
        assert!(!spec.disabled);
        assert_eq!(
            world.get::<Transform>(driver).unwrap().translation.x,
            1.0,
            "the script must read back its own style write within the frame"
        );
    }

    #[test]
    fn script_reads_and_moves_ui_focus() {
        let dir = std::env::temp_dir().join("floptle_script_test_ui_focus");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "menu",
            concat!(
                "function update(node, dt)\n",
                "  local play = find(\"Play\")\n",
                "  local quit = find(\"Quit\")\n",
                // Read the engine's focus, two ways.
                "  node.x = play.focused and 1 or 0\n",
                "  node.y = (ui.focused() ~= nil) and 1 or 0\n",
                // Move it, then read the move back within the same frame.
                "  ui.focus(quit)\n",
                "  node.z = quit.focused and 1 or 0\n",
                "end\n",
            ),
        );
        let mut world = World::default();
        let driver = world.spawn();
        world.insert(driver, Transform::IDENTITY);
        world.insert(
            driver,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "menu".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut el = |name: &str| {
            let e = world.spawn();
            world.insert(e, Transform::IDENTITY);
            world.insert(e, floptle_core::Name(name.into()));
            world.insert(
                e,
                floptle_ui::ElementSpec { focusable: true, ..Default::default() },
            );
            e
        };
        let play = el("Play");
        let quit = el("Quit");

        let mut host = ScriptHost::new();
        // The engine publishes the focus before the run, exactly as the
        // interact pass does.
        host.set_ui_focus(Some(play.index()));
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let t = world.get::<Transform>(driver).unwrap().translation;
        assert_eq!(t.x, 1.0, "node.focused sees the engine's focus");
        assert_eq!(t.y, 1.0, "ui.focused() returns a node");
        assert_eq!(t.z, 1.0, "ui.focus() reads back within the same frame");
        // …and the engine gets the request out.
        assert_eq!(host.take_ui_focus_request(), Some(Some(quit.index())));
        assert_eq!(host.take_ui_focus_request(), None, "draining is one-shot");
    }

    #[test]
    fn a_script_paints_with_colors_and_reads_booleans_as_booleans() {
        let dir = std::env::temp_dir().join("floptle_script_test_ui_color");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "paint",
            concat!(
                "function update(node, dt)\n",
                "  local el = node:getcomponent(\"UiElement\")\n",
                // One line instead of four channel pokes.
                "  el.fill = color(1, 0.5, 0.25)\n",
                "  el.textColor = color.hex(\"#3366ccff\")\n",
                // …and a boolean that behaves like one. `visible` starts
                // false; if it read back as the number 0 this branch would be
                // taken, because 0 is truthy in Lua. That is the bug.
                "  if el.visible then node.x = 99 else node.x = 1 end\n",
                "  el.visible = true\n",
                "end\n",
            ),
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            floptle_ui::ElementSpec {
                visible: false,
                shape: Some(floptle_ui::ShapeSpec::default()),
                text: Some(floptle_ui::TextSpec { text: "hi".into(), ..Default::default() }),
                ..Default::default()
            },
        );
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "paint".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(
            world.get::<Transform>(e).unwrap().translation.x,
            1.0,
            "`if el.visible` must be false when it is false — 0 is truthy in Lua"
        );
        let spec = world.get::<floptle_ui::ElementSpec>(e).unwrap();
        let fill = spec.shape.as_ref().unwrap().fill;
        assert!((fill[0] - 1.0).abs() < 1e-6 && (fill[1] - 0.5).abs() < 1e-6);
        assert_eq!(fill[3], 1.0, "a three-argument color is opaque, not invisible");
        let tc = spec.text.as_ref().unwrap().color;
        assert!((tc[0] - 0x33 as f32 / 255.0).abs() < 1e-4, "hex parsed: {tc:?}");
        assert!(spec.visible);
    }

    #[test]
    fn ui_bind_keeps_a_label_and_a_bar_up_to_date() {
        let dir = std::env::temp_dir().join("floptle_script_test_ui_bind");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "hud",
            concat!(
                "hp = 10\n",
                "function start(node)\n",
                // Say the relationship once, in `start` — not an `update` per
                // label that has to be kept true by hand.
                "  ui.bind(find(\"Label\"), \"text\", function() return \"HP \" .. hp end)\n",
                "  ui.bind(find(\"Bar\"), \"value\", function() return hp / 20 end)\n",
                "  ui.bind(find(\"Label\"), \"textColor\",\n",
                "          function() return hp >= 5 and color(1,1,1) or color(1,0,0) end)\n",
                "end\n",
                "function update(node, dt)\n  hp = hp - 5\nend\n",
            ),
        );
        let mut world = World::default();
        let driver = world.spawn();
        world.insert(driver, Transform::IDENTITY);
        world.insert(
            driver,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "hud".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let label = world.spawn();
        world.insert(label, Transform::IDENTITY);
        world.insert(label, floptle_core::Name("Label".into()));
        world.insert(
            label,
            floptle_ui::ElementSpec {
                text: Some(floptle_ui::TextSpec { text: "?".into(), ..Default::default() }),
                ..Default::default()
            },
        );
        let bar = world.spawn();
        world.insert(bar, Transform::IDENTITY);
        world.insert(bar, floptle_core::Name("Bar".into()));
        world.insert(
            bar,
            floptle_ui::ElementSpec {
                slider: Some(floptle_ui::SliderSpec { value: 0.0, ..Default::default() }),
                ..Default::default()
            },
        );

        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        // Bindings run after every `update`, so the first frame already shows
        // the value this frame produced, not the one it started with.
        assert_eq!(world.get::<floptle_ui::ElementSpec>(label).unwrap().text.as_ref().unwrap().text, "HP 5");
        let v = world.get::<floptle_ui::ElementSpec>(bar).unwrap().slider.unwrap().value;
        assert!((v - 0.25).abs() < 1e-6, "the bar found UiSlider.value, not UiElement: {v}");
        assert_eq!(
            world.get::<floptle_ui::ElementSpec>(label).unwrap().text.as_ref().unwrap().color,
            [1.0, 1.0, 1.0, 1.0]
        );

        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert_eq!(world.get::<floptle_ui::ElementSpec>(label).unwrap().text.as_ref().unwrap().text, "HP 0");
        assert_eq!(
            world.get::<floptle_ui::ElementSpec>(label).unwrap().text.as_ref().unwrap().color,
            [1.0, 0.0, 0.0, 1.0],
            "the colour binding re-evaluated too"
        );
    }

    #[test]
    fn a_binding_that_throws_is_dropped_rather_than_reported_sixty_times_a_second() {
        let dir = std::env::temp_dir().join("floptle_script_test_ui_bind_err");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "bad",
            concat!(
                "function start(node)\n",
                "  ui.bind(find(\"Label\"), \"text\", function() error(\"nope\") end)\n",
                "end\n",
            ),
        );
        let mut world = World::default();
        let driver = world.spawn();
        world.insert(driver, Transform::IDENTITY);
        world.insert(
            driver,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "bad".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let label = world.spawn();
        world.insert(label, Transform::IDENTITY);
        world.insert(label, floptle_core::Name("Label".into()));
        world.insert(
            label,
            floptle_ui::ElementSpec {
                text: Some(floptle_ui::TextSpec::default()),
                ..Default::default()
            },
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert_eq!(host.errors().len(), 1, "reported once: {:?}", host.errors());
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "…and not again: {:?}", host.errors());
    }

    /// FIELD REGRESSION (floptle/0048): a node in `script_skip` never gets a
    /// late pass, and a client's join sequence puts every rollback fighter
    /// there before the driver exists to claim it back.
    ///
    /// `script_skip` gates EVERY pass; `driver_skip` gates all but `lateUpdate`,
    /// because no driver replays the late pass. The join sequence writes the
    /// first and the rollback start writes the second, and for two releases
    /// nothing took the fighters back out of the first — so the fight ran and
    /// the cosmetic pass silently did not, on the client only.
    #[test]
    fn a_driver_owned_node_keeps_its_late_pass_after_the_session_filtered_it() {
        let dir = std::env::temp_dir().join("floptle_script_test_late_filter");
        let _ = std::fs::create_dir_all(&dir);
        // `fixedUpdate` writes one value, `lateUpdate` writes another over it —
        // the same shape the field report measured with (+0.25 on top).
        write_script(
            &dir,
            "facing",
            concat!(
                "function fixedUpdate(node, dt)\n  node.y = 1\nend\n",
                "function lateUpdate(node, dt)\n  node.y = 2\nend\n",
            ),
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "facing".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);

        // Step 2 of the join: no driver yet, so the session classifies the
        // fighter as an ordinary synced node and filters it out of everything.
        host.set_script_filter(std::collections::HashSet::from([e.index()]));
        host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
        host.run_late(&mut world, 1.0 / 60.0, 0.0);
        assert_eq!(
            world.get::<Transform>(e).unwrap().translation.y,
            0.0,
            "the un-driven window is supposed to skip everything; if it doesn't, \
             this test proves nothing"
        );

        // Step 3: the driver binds it. `extend_filters` ALONE is the bug —
        // `script_skip` still holds it, so the late pass stays dead.
        host.extend_filters([e.index()]);
        host.run_late(&mut world, 1.0 / 60.0, 0.0);
        assert_eq!(
            world.get::<Transform>(e).unwrap().translation.y,
            0.0,
            "reproducing the bug: extend_filters does not undo script_skip"
        );

        // The fix: the rollback start takes the session's half back out first.
        host.shrink_filters([e.index()]);
        host.extend_filters([e.index()]);
        host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
        assert_eq!(
            world.get::<Transform>(e).unwrap().translation.y,
            0.0,
            "the driver still owns the TICK — the global fixedUpdate stays off"
        );
        host.run_late(&mut world, 1.0 / 60.0, 0.0);
        assert_eq!(
            world.get::<Transform>(e).unwrap().translation.y,
            2.0,
            "…and lateUpdate runs again, which is the whole point"
        );
    }

    /// floptle/0052: `node.texture = "..."` did NOTHING — not an error, not a
    /// warning, no return value. A character-select strip assigned portraits
    /// that way for months and showed the placeholder on every slot.
    #[test]
    fn script_sets_and_reads_a_ui_element_texture() {
        let dir = std::env::temp_dir().join("floptle_script_test_ui_texture");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "portrait",
            "function update(node, dt)\n  \
               node.texture = \"textures/ui/sae.png\"\n  \
               readback = node.texture\n\
             end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        // A bare element with NO image slot — the write has to create one, the
        // way a sprite frame-swap track does.
        world.insert(e, floptle_ui::ElementSpec::default());
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "portrait".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let spec = world.get::<floptle_ui::ElementSpec>(e).expect("element");
        assert_eq!(
            spec.image.as_ref().map(|i| i.texture.as_str()),
            Some("textures/ui/sae.png"),
            "the write must reach the ECS, not vanish"
        );
    }

    /// The shape queries exist as globals and answer from Lua — the Rust unit
    /// tests prove the geometry, this proves a script can actually reach it.
    #[test]
    fn shape_queries_are_callable_from_lua() {
        let dir = std::env::temp_dir().join("floptle_script_test_shape_api");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "probe",
            "function update(node, dt)\n  \
               kinds = type(overlapSphere) .. type(spherecast) .. type(capsulecast)\n  \
               n = #overlapSphere(vec3(0, 0, 0), 5)\n  \
               miss = spherecast(vec3(0, 0, 0), vec3(1, 0, 0), 0.5, 10)\n\
             end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "probe".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        // No colliders lent, so the answers are "nothing" — but they must be
        // ANSWERS (an empty list, a nil) rather than an error about a missing
        // global, which is what a query nobody wired would give.
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    }

    /// The other half: a non-string raises instead of being dropped. A write
    /// that silently does nothing is the disease; the wrong portrait was the
    /// symptom.
    #[test]
    fn a_non_string_texture_raises() {
        let dir = std::env::temp_dir().join("floptle_script_test_ui_texture_bad");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "bad", "function update(node, dt)\n  node.texture = 42\nend\n");
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, floptle_ui::ElementSpec::default());
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "bad".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(
            host.errors().iter().any(|e| e.contains("texture")),
            "a bad texture write must say so: {:?}",
            host.errors()
        );
    }

    #[test]
    fn script_applies_material_preset() {
        // node.material = "<name>" resolves against the lent presets and inserts a Material.
        let dir = std::env::temp_dir().join("floptle_script_test_material");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "paint", "function update(node, dt)\n  node.material = \"Gold\"\nend\n");
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Matter::Mesh { asset_path: "m.glb".into() });
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "paint".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
        );
        let mut host = ScriptHost::new();
        let mut mats = HashMap::new();
        mats.insert("Gold".to_string(), Material::tinted([1.0, 0.84, 0.0]));
        host.set_materials(mats);
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let mat = world.get::<Material>(e).expect("material applied");
        assert_eq!(mat.color, [1.0, 0.84, 0.0]);
    }

    #[test]
    fn script_reads_and_writes_a_component_field() {
        // node:getcomponent("PointLight") reads the light's live fields, and assigning one
        // flushes back to the ECS the same frame.
        let dir = std::env::temp_dir().join("floptle_script_test_component");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "oscillate",
            "function update(node, dt)\n  local l = node:getcomponent(\"PointLight\")\n  if l then l.intensity = l.intensity + 1.0 end\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Matter::PointLight { color: [1.0, 1.0, 1.0], intensity: 2.0, range: 10.0 });
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "oscillate".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        match world.get::<Matter>(e).unwrap() {
            Matter::PointLight { intensity, .. } => {
                assert!((*intensity - 3.0).abs() < 1e-4, "intensity became {intensity}, expected 3.0")
            }
            other => panic!("expected point light, got {other:?}"),
        }
    }

    #[test]
    fn script_tunes_every_rigidbody_field() {
        // Every Inspector tunable on a Rigidbody is scriptable: read the mirror,
        // assign new values (booleans allowed), and the ECS component reflects
        // them after the same run() — which is what the live sim re-reads.
        let dir = std::env::temp_dir().join("floptle_script_test_rigidbody");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "ice",
            "function update(node, dt)\n\
             local rb = node:getcomponent(\"RigidBody\")\n\
             rb.friction = 0.02\n\
             rb.restitution = 0.9\n\
             rb.gravity = false\n\
             rb.shape = 2\n\
             rb.radius = 1.5\n\
             rb.height = 3.0\n\
             rb.half_x = 0.25\n\
             rb.half_y = 0.5\n\
             rb.half_z = 0.75\n\
             rb.lock_z = true\n\
             rb.lock_rot_x = true\n\
             rb.lock_rot_z = 1\n\
             if rb.lock_y then rb.friction = -1 end -- reads back as a BOOLEAN, and is false\n\
            end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, RigidBody::default());
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "ice".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let rb = world.get::<RigidBody>(e).unwrap();
        assert!((rb.friction - 0.02).abs() < 1e-4, "friction = {}", rb.friction);
        assert!((rb.restitution - 0.9).abs() < 1e-4);
        assert!(!rb.gravity);
        assert_eq!(rb.kind, floptle_core::BodyKind::Box);
        assert!((rb.radius - 1.5).abs() < 1e-4);
        assert!((rb.height - 3.0).abs() < 1e-4);
        assert_eq!(rb.half_extents, [0.25, 0.5, 0.75]);
        assert_eq!(rb.lock_pos, [false, false, true]);
        assert_eq!(rb.lock_rot, [true, false, true]);
    }

    #[test]
    fn script_toggles_visibility() {
        // node.visible reads true by default; assigning false attaches Visible(false).
        let dir = std::env::temp_dir().join("floptle_script_test_visible");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "hide",
            "function update(node, dt)\n  if node.visible then node.visible = false end\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Matter::Mesh { asset_path: "m.glb".into() });
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "hide".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(world.get::<Visible>(e).copied(), Some(Visible(false)));
    }

    #[test]
    fn layers_and_tags_round_trip_through_the_lua_api() {
        // node.layer reads "Default" when unset; a valid write lands as a
        // Layer component; tags edit read-your-writes and flush as Tags; a
        // findTagged scan sees a PRE-EXISTING tag the same frame.
        let dir = std::env::temp_dir().join("floptle_script_test_layers");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "layerer",
            "function update(node, dt)\n\
             local score = 0\n\
             if node.layer == \"Default\" then score = score + 1 end\n\
             node.layer = \"Enemies\"\n\
             if node.layer == \"Enemies\" then score = score + 10 end\n\
             node:addTag(\"boss\")\n\
             node:addTag(\"boss\")\n\
             if node:hasTag(\"boss\") and #node.tags == 1 then score = score + 100 end\n\
             if #findTagged(\"marked\") == 1 then score = score + 1000 end\n\
             local ok, err = pcall(function() node.layer = \"Typo\" end)\n\
             if not ok then score = score + 10000 end\n\
             node.x = score\n\
            end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "layerer".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
        );
        let marked = world.spawn();
        world.insert(marked, Transform::IDENTITY);
        world.insert(marked, floptle_core::Tags(vec!["marked".into()]));
        let mut host = ScriptHost::new();
        host.set_layers(floptle_core::Layers::resolve(
            vec!["Default".into(), "Enemies".into()],
            &[],
        ));
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 11111.0);
        assert_eq!(
            world.get::<floptle_core::Layer>(e).map(|l| l.0.clone()),
            Some("Enemies".to_string())
        );
        assert_eq!(
            world.get::<floptle_core::Tags>(e).map(|t| t.0.clone()),
            Some(vec!["boss".to_string()])
        );
    }

    /// vec3/vec2 value types + distance: constructors, operators, methods,
    /// node interop (`distance(node, other)`, `node.pos` read/write).
    #[test]
    fn vector_math_and_distance_work_end_to_end() {
        let dir = std::env::temp_dir().join("floptle_script_test_vecmath");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "vectors",
            "function update(node, dt)\n\
               local score = 0\n\
               local a = vec3(1, 2, 2)\n\
               if a:length() == 3 then score = score + 1 end\n\
               local b = a + vec3(1)\n\
               if b.x == 2 and b.y == 3 and b.z == 3 then score = score + 10 end\n\
               if (a * 2):length() == 6 then score = score + 100 end\n\
               if vec3(2,0,0):normalized() == vec3(1,0,0) then score = score + 1000 end\n\
               if vec3(1,0,0):cross(vec3(0,1,0)).z == 1 then score = score + 10000 end\n\
               if vec3(0,0,0):lerp(vec3(10,0,0), 0.5).x == 5 then score = score + 100000 end\n\
               if distance(vec3(0,0,0), vec3(3,4,0)) == 5 then score = score + 1000000 end\n\
               local target = find(\"Target\")\n\
               if distance(node, target) == 7 then score = score + 10000000 end\n\
               if vec2(3, 4):length() == 5 then score = score + 100000000 end\n\
               node.pos = vec3(score, node.pos.y, 0)\n\
             end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "vectors".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let target = world.spawn();
        world.insert(target, Transform::from_translation(glam::DVec3::new(0.0, 7.0, 0.0)));
        world.insert(target, floptle_core::Name("Target".into()));
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.016, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 111111111.0);
    }

    /// Collision/trigger hooks: `call_touch` dispatches to a script's
    /// `onCollisionEnter(node, other, hit)` with the other node's handle and
    /// the contact info — and never mis-fires a hook the script doesn't define.
    #[test]
    fn touch_dispatch_reaches_the_hook_with_other_and_hit() {
        let dir = std::env::temp_dir().join("floptle_script_test_touch");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "bumper",
            "function update(node, dt) end\n\
             function onCollisionEnter(node, other, hit)\n\
               -- prove we got the right other node + contact info\n\
               if other.name == \"Wall\" and hit.ny == 1 then\n\
                 node.x = hit.x + 100\n\
               end\n\
             end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "bumper".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let wall = world.spawn();
        world.insert(wall, Transform::IDENTITY);
        world.insert(wall, floptle_core::Name("Wall".into()));
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.016, 0.0); // build envs + mirror
        host.call_touch(&mut world, e.index(), "onCollisionEnter", wall.index(), [7.0, 0.0, 0.0], [
            0.0, 1.0, 0.0,
        ]);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 107.0);
        // An undefined hook is a clean no-op.
        host.call_touch(&mut world, e.index(), "onTriggerEnter", wall.index(), [0.0; 3], [0.0; 3]);
        assert!(host.errors().is_empty());
    }

    /// `spawn(prefab, pos, fn)` queues a request (with the position and the
    /// callback), `destroy(node)` / `node:destroy()` queue entity indices, and
    /// the driver-invoked callback configures the freshly spawned node.
    #[test]
    fn spawn_and_destroy_queue_and_callback_configures_the_new_node() {
        let dir = std::env::temp_dir().join("floptle_script_test_spawn");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "spawner",
            "function update(node, dt)\n\
               if not done then\n\
                 done = true\n\
                 spawn(\"bullet\", vec3(1, 2, 3), function(b)\n\
                   b.x = 42\n\
                 end)\n\
                 destroy(node)\n\
                 local victim = find(\"Victim\")\n\
                 victim:destroy()\n\
               end\n\
             end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "spawner".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        world.insert(e, floptle_core::Matter::Empty);
        let victim = world.spawn();
        world.insert(victim, Transform::IDENTITY);
        world.insert(victim, floptle_core::Name("Victim".into()));
        world.insert(victim, floptle_core::Matter::Empty);
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.016, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

        let mut spawns = host.take_spawn_requests();
        assert_eq!(spawns.len(), 1);
        let req = spawns.remove(0);
        assert_eq!(req.prefab, "bullet");
        assert_eq!(req.pos, Some([1.0, 2.0, 3.0]));
        let destroys = host.take_destroy_requests();
        assert_eq!(destroys, vec![e.index(), victim.index()], "both destroy forms queue");
        assert!(host.take_spawn_requests().is_empty(), "drain empties the queue");

        // The driver spawns the prefab (simulated here) and hands the callback
        // the new root — its writes flush straight to the ECS.
        let bullet = world.spawn();
        world.insert(bullet, Transform::IDENTITY);
        world.insert(bullet, floptle_core::Name("bullet".into()));
        world.insert(bullet, floptle_core::Matter::Empty);
        host.call_spawn_callback(&mut world, req.cb.expect("callback captured"), bullet.index());
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(world.get::<Transform>(bullet).unwrap().translation.x, 42.0);
    }

    #[test]
    fn assets_api_resolves_under_project_root() {
        // assets.getFile returns the path for an existing file (nil for a missing one);
        // assets.getContents lists a directory. Encode the three results into node.x.
        let root = std::env::temp_dir().join("floptle_script_test_assets_root");
        let models = root.join("models");
        let _ = std::fs::create_dir_all(&models);
        let _ = std::fs::write(models.join("armor.glb"), b"x");
        let dir = std::env::temp_dir().join("floptle_script_test_assets_scripts");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "probe",
            "function update(node, dt)\n  local f = assets.getFile(\"models/armor.glb\")\n  local missing = assets.getFile(\"models/nope.glb\")\n  local c = assets.getContents(\"models\")\n  node.x = (f ~= nil and 1 or 0) + (missing == nil and 10 or 0) + (#c == 1 and 100 or 0)\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "probe".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
        );
        let mut host = ScriptHost::new();
        host.set_project_root(root);
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 111.0);
    }

    #[test]
    fn save_api_round_trips_across_hosts() {
        // set → flush writes save/<slot>.ron; a FRESH host (a new play session /
        // process) reads the same values back. Tables survive; defaults fill gaps.
        let root = std::env::temp_dir().join("floptle_script_test_save_root");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::create_dir_all(&root);
        let dir = std::env::temp_dir().join("floptle_script_test_save_scripts");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "writer",
            "function update(node, dt)\n  save.set(\"gold\", 42)\n  save.set(\"who\", {name=\"Ty\", hp=7})\n  save.flush()\nend\n",
        );
        write_script(
            &dir,
            "reader",
            "function update(node, dt)\n  local who = save.get(\"who\")\n  node.x = save.get(\"gold\", 0) + (who and who.hp or 0) * 1000 + save.get(\"missing\", 5)\nend\n",
        );
        let run = |kind: &str| -> f64 {
            let mut world = World::default();
            let e = world.spawn();
            world.insert(e, Transform::IDENTITY);
            world.insert(
                e,
                Scripts(vec![floptle_core::ScriptInst {
                    kind: kind.into(),
                    enabled: true,
                    params: vec![],
                    refs: Vec::new(),
                    strs: Vec::new(),
                }]),
            );
            let mut host = ScriptHost::new();
            host.set_project_root(root.clone());
            host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
            assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
            world.get::<Transform>(e).unwrap().translation.x
        };
        run("writer");
        assert!(root.join("save/main.ron").exists(), "flush wrote the slot file");
        assert_eq!(run("reader"), 42.0 + 7000.0 + 5.0);
    }

    /// Position writes on BODY nodes must queue real teleports — the physics
    /// writeback stomps bare transform writes next frame, which silently ate
    /// respawns ("G restores the ship… nothing moves") and the parked-in-hull
    /// astronaut. Both write paths: own-node raw fields AND cross-node handles.
    #[test]
    fn body_position_writes_queue_teleports() {
        let dir = std::env::temp_dir().join("floptle_script_test_teleport");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "teleporter",
            "function fixedUpdate(node, dt)\n\
               node.y = 50.0\n\
               local buddy = find(\"Buddy\")\n\
               if buddy then buddy.x = 7.0 end\n\
             end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "teleporter".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        world.insert(e, floptle_core::Name("Pilot".into()));
        let buddy = world.spawn();
        world.insert(buddy, Transform::IDENTITY);
        world.insert(buddy, floptle_core::Name("Buddy".into()));
        let mut host = ScriptHost::new();
        // Both entities HAVE bodies this tick (the gate for teleport queuing).
        let mut states = HashMap::new();
        for eid in [e.index(), buddy.index()] {
            states.insert(eid, BodyState::default());
        }
        host.set_bodies(states.clone());
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        host.set_bodies(states);
        host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let tp = host.take_body_pos_changes();
        assert_eq!(
            tp.get(&e.index()).map(|p| p[1]),
            Some(50.0),
            "own-node position write must queue a body teleport (got {tp:?})"
        );
        assert_eq!(
            tp.get(&buddy.index()).map(|p| p[0]),
            Some(7.0),
            "cross-node handle position write must queue a body teleport (got {tp:?})"
        );
    }

    /// A4 scheduler: tick-driven determinism, cancel, tween endpoints — and the
    /// invariant that targeted replays (`run_fixed_for`) do NOT advance timers
    /// (netcode prediction re-runs one entity's tick; a scheduler advancing
    /// there would double-fire everything pending).
    #[test]
    fn scheduler_fires_on_ticks_and_ignores_replays() {
        let dir = std::env::temp_dir().join("floptle_script_test_sched");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "sched",
            "local fired, everies, tw_last, tw_calls = 0, 0, -1, 0\n\
             local cancelled_ran = false\n\
             function start(node)\n\
               after(0.045, function() fired = fired + 1 end)\n\
               local h = after(0.045, function() cancelled_ran = true end)\n\
               h:cancel()\n\
               every(0.095, function() everies = everies + 1 end)\n\
               tween(0.1, function(a) tw_last = a; tw_calls = tw_calls + 1 end, \"smooth\")\n\
             end\n\
             function update(node, dt)\n\
               node.x = fired\n\
               node.y = everies + (cancelled_ran and 100 or 0)\n\
               node.z = tw_last * 1000 + tw_calls\n\
             end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "sched".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        let dt = 1.0 / 60.0;
        host.run(&mut world, &dir, dt, 0.0); // start() schedules everything
        // 30 global ticks = 0.5s: after(0.045) fired once, every(0.095) fired 5
        // times (0.095, 0.19, 0.285, 0.38, 0.475 — periods deliberately OFF the
        // tick grid so f64 accumulation can't make the count edge-dependent),
        // and the 0.1s tween completed, ending exactly at eased(1.0) = 1.0.
        for i in 0..30 {
            host.run_fixed(&mut world, dt, i as f32 * dt);
        }
        // Replays must not advance the clock: this would double-fire everything.
        for _ in 0..100 {
            host.run_fixed_for(&mut world, e.index(), dt, 0.5);
        }
        host.run(&mut world, &dir, dt, 0.5); // update() copies counters out
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let t = world.get::<Transform>(e).unwrap().translation;
        assert_eq!(t.x, 1.0, "after() must fire exactly once (got {})", t.x);
        assert_eq!(
            t.y, 5.0,
            "every(0.095) over 0.5s = 5 fires, cancelled timer never runs (got {})",
            t.y
        );
        let (final_alpha, tw_calls) = ((t.z as i32) / 1000, (t.z as i32) % 1000);
        assert_eq!(final_alpha, 1, "tween's final alpha must be exactly 1.0 (z = {})", t.z);
        assert!(
            (6..=8).contains(&tw_calls),
            "a 0.1s tween at 60Hz is ~7 per-tick calls, then stops (got {tw_calls})"
        );
    }

    fn hull(eid: u32, x: f32) -> floptle_physics::BodyHull {
        floptle_physics::BodyHull {
            eid,
            pos: glam::Vec3::new(x, 0.0, 0.0),
            radius: 0.4,
            shape: floptle_physics::BodyShape::Capsule { half_height: 0.6 },
            up: glam::Vec3::Y,
            layer: 0,
        }
    }

    #[test]
    fn raycast_hits_body_hulls_with_node_identity_and_self_exclusion() {
        let dir = std::env::temp_dir().join("floptle_script_test_hulls");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "caster",
            "function update(node, dt)\n\
               -- the explicit ignore makes the only other hull invisible too\n\
               if raycast(0, 0, 0, 1, 0, 0, 50, params.targetid) == nil then\n\
                 node.scale = 3\n\
               end\n\
               local hit = raycast(node.x, node.y, node.z, 1, 0, 0, 50)\n\
               if hit then\n\
                 node.y = hit.distance\n\
                 if hit.node then node.z = 42 end\n\
               end\n\
               net.rpc(\"swing\", { dir = 1 }, { withInput = true })\n\
             end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "caster".into(),
                enabled: true,
                params: vec![("targetid".into(), (e.index() + 1000) as f32)], refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        // The caster's OWN hull sits at its position — without self-exclusion
        // the ray would hit it at distance 0.
        host.set_hulls(vec![hull(e.index(), 0.0), hull(e.index() + 1000, 5.0)]);
        host.run(&mut world, &dir, 0.01, 0.01);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let tr = world.get::<Transform>(e).unwrap();
        assert!(
            (tr.translation.y - 4.6).abs() < 0.05,
            "must hit the OTHER hull's surface (5 − 0.4), not itself: {}",
            tr.translation.y
        );
        assert_eq!(tr.translation.z, 42.0, "a body hit must carry hit.node");
        assert_eq!(tr.scale.x, 3.0, "the explicit `ignore` arg must skip that body");
        // `{withInput = true}` reaches the command queue.
        let cmds = host.take_net_commands();
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                NetCmd::Rpc { name, with_input: true, .. } if name == "swing"
            )),
            "withInput must ride the rpc command: {cmds:?}"
        );
    }

    #[test]
    fn second_script_on_a_body_node_must_not_clobber_velocity_writes() {
        // A movement controller sets the velocity; a weapon script on the SAME
        // node never touches it. The weapon's pass must not write the stale
        // seeded velocity back over the controller's (the sliding-player bug).
        let dir = std::env::temp_dir().join("floptle_script_test_two_scripts");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "mover", "function update(node, dt)\n  node.vx = 5\n  node.vy = 7\nend\n");
        write_script(&dir, "weapon", "function update(node, dt)\n  -- looks at the node, never writes velocity\n  local _ = node.vx\nend\n");
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![
                floptle_core::ScriptInst { kind: "mover".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() },
                floptle_core::ScriptInst { kind: "weapon".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() },
            ]),
        );
        let mut host = ScriptHost::new();
        // The body's pre-hook state this frame (what node.vx is seeded with).
        let mut bodies = HashMap::new();
        bodies.insert(
            e.index(),
            BodyState { vel: [0.0, -2.0, 0.0], grounded: true, ..Default::default() },
        );
        host.set_bodies(bodies);
        host.run(&mut world, &dir, 0.016, 0.016);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let changes = host.take_body_changes();
        assert_eq!(
            changes.get(&e.index()),
            Some(&[5.0, 7.0, 0.0f32]),
            "the controller's write must survive the weapon's pass"
        );
        // And a script that touches nothing queues nothing.
        assert!(host.take_body_height_changes().is_empty(), "untouched height must not queue");
    }

    #[test]
    fn is_mine_and_find_scripts_pick_the_local_player() {
        // Two identical avatars, one probe: findScripts enumerates every
        // instance and net.isMine tells which one THIS machine controls —
        // how a shared camera finds the local player among many avatars.
        let dir = std::env::temp_dir().join("floptle_script_test_ismine");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "avatar", "function update(node, dt) end\n");
        write_script(
            &dir,
            "probe",
            "function update(node, dt)\n\
               local list = findScripts(\"avatar\")\n\
               node.z = #list\n\
               for i, s in ipairs(list) do\n\
                 if net.isMine(s.node) then node.x = i end\n\
               end\n\
               node.y = net.isMine(node) and 1 or 0\n\
             end\n",
        );
        let mut world = World::default();
        let avatar = |w: &mut World, x: f64| {
            let e = w.spawn();
            w.insert(
                e,
                Transform::from_translation(floptle_core::math::DVec3::new(x, 0.0, 0.0)),
            );
            w.insert(
                e,
                Scripts(vec![floptle_core::ScriptInst {
                    kind: "avatar".into(),
                    enabled: true,
                    params: vec![], refs: Vec::new(),
                    strs: Vec::new(),
                }]),
            );
            e
        };
        let a1 = avatar(&mut world, 0.0);
        let a2 = avatar(&mut world, 10.0);
        let probe = world.spawn();
        world.insert(probe, Transform::IDENTITY);
        world.insert(
            probe,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "probe".into(),
                enabled: true,
                params: vec![], refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        let mut owners = HashMap::new();
        owners.insert(a1.index(), None); // networked, host-owned
        owners.insert(a2.index(), Some(2u64)); // networked, peer 2's avatar
        host.set_net_owners(owners);

        // On the SERVER: the unowned avatar is mine; peer 2's is not.
        host.set_net_state(NetState {
            role: NetRoleState::Server,
            peers: vec![2],
            rtt_ms: 0.0,
            my_peer: None,
            ..Default::default()
        });
        host.run(&mut world, &dir, 0.016, 0.016);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let tr = world.get::<Transform>(probe).unwrap();
        assert_eq!(tr.translation.z, 2.0, "findScripts must list both avatars");
        assert_eq!(tr.translation.x, 1.0, "server: the unowned avatar is mine");
        assert_eq!(tr.translation.y, 1.0, "non-networked nodes are mine everywhere");

        // As CLIENT peer 2: only my own avatar is mine.
        host.set_net_state(NetState {
            role: NetRoleState::Client,
            peers: vec![],
            rtt_ms: 0.0,
            my_peer: Some(2),
            ..Default::default()
        });
        host.run(&mut world, &dir, 0.016, 0.032);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(
            world.get::<Transform>(probe).unwrap().translation.x,
            2.0,
            "client: peer 2 owns avatar 2"
        );
    }

    #[test]
    fn net_rewind_swaps_poses_and_synced_vars_then_restores() {
        let dir = std::env::temp_dir().join("floptle_script_test_rewind");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "judge",
            "replicated = { parrying = false }\n\
             onRpc = {}\n\
             function onRpc.swing(args, sender)\n\
               net.rewind(sender, function()\n\
                 local hit = raycast(0, 0, 0, 1, 0, 0, 50)\n\
                 node.x = hit and hit.distance or -1\n\
                 node.y = synced.parrying and 1 or 0\n\
               end)\n\
               local live = raycast(0, 0, 0, 1, 0, 0, 50)\n\
               node.z = live and live.distance or -1\n\
             end\n\
             function update(node, dt) end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "judge".into(),
                enabled: true,
                params: vec![], refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.set_net_state(NetState { role: NetRoleState::Server, peers: vec![7], rtt_ms: 0.0, my_peer: None, ..Default::default() });
        host.run(&mut world, &dir, 0.01, 0.01); // instantiate
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

        // A target LIVE at x = 10; the sender perceived it at x = 5, parrying.
        host.set_hulls(vec![hull(999, 10.0)]);
        host.set_rewind(Some(RewindScope {
            peer: 7,
            poses: vec![(999, [5.0, 0.0, 0.0])],
            synced: vec![(
                e.index(),
                "judge".into(),
                vec![("parrying".into(), floptle_net::NetValue::Bool(true))],
            )],
        }));
        host.dispatch_rpc(&mut world, "swing", &floptle_net::NetValue::Nil, 7);
        host.set_rewind(None);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let tr = world.get::<Transform>(e).unwrap();
        assert!(
            (tr.translation.x - 4.6).abs() < 0.05,
            "inside rewind the hull sits at the PERCEIVED x=5: {}",
            tr.translation.x
        );
        assert_eq!(tr.translation.y, 1.0, "synced.parrying reads the rewound tick's value");
        assert!(
            (tr.translation.z - 9.6).abs() < 0.05,
            "after rewind the live pose is back (x=10): {}",
            tr.translation.z
        );
        // The live synced store was restored too.
        let collected = host.collect_synced();
        assert_eq!(
            collected[0].2[0],
            ("parrying".to_string(), floptle_net::NetValue::Bool(false)),
            "rewind must not leak historical values into the present"
        );

        // Without a staged scope, rewind warns and runs at server time.
        host.drain_logs();
        host.dispatch_rpc(&mut world, "swing", &floptle_net::NetValue::Nil, 7);
        let tr = world.get::<Transform>(e).unwrap();
        assert!((tr.translation.x - 9.6).abs() < 0.05, "no scope ⇒ live pose");
        assert!(
            host.drain_logs().iter().any(|l| l.msg.contains("no lag-comp context")),
            "the fallback must be loud"
        );
    }

    #[test]
    fn string_field_applier_swaps_ui_image() {
        // The animation system's property tracks apply through these. A UI
        // image swap is the headline case (sprite frame-swapping).
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, floptle_ui::ElementSpec::default());

        // No image slot yet → the applier creates one.
        crate::apply_component_field_str(&mut world, e, "UiElement", "image", "textures/a.png");
        let img = world.get::<floptle_ui::ElementSpec>(e).unwrap().image.clone().unwrap();
        assert_eq!(img.texture, "textures/a.png");

        // A later frame swaps the texture on the existing slot.
        crate::apply_component_field_str(&mut world, e, "UiElement", "image", "textures/b.png");
        let img = world.get::<floptle_ui::ElementSpec>(e).unwrap().image.clone().unwrap();
        assert_eq!(img.texture, "textures/b.png");

        // The numeric applier still drives opacity on the same element.
        crate::apply_component_field(&mut world, e, "UiElement", "opacity", 0.5);
        assert_eq!(world.get::<floptle_ui::ElementSpec>(e).unwrap().opacity, 0.5);
    }

    /// `anim:events` / `anim:duration` expose the AUTHORED clip data so a game can bake
    /// integer frame data at load, instead of letting float playback events drive
    /// gameplay (which stepped playback quantises and a prediction replay never re-fires).
    /// They read the asset mirror, so they answer in `start()` — before anything has
    /// played a frame (floptle/0023).
    #[test]
    fn animator_exposes_authored_clip_events_and_duration() {
        let dir = std::env::temp_dir().join("floptle_script_test_anim_events");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "bake",
            "function start(node)\n\
               local a = node:animator()\n\
               local dur = a:duration('Punch')\n\
               local evs = a:events('Punch')\n\
               -- 12 gameplay frames over the clip: which frame is the hitbox on?\n\
               for _, e in ipairs(evs) do\n\
                 if e.func == 'onHitboxStart' then\n\
                   node.x = math.floor(e.t / dur * 12 + 0.5)\n\
                 end\n\
               end\n\
               node.y = #evs\n\
               node.z = (a:events('NoSuchClip') == nil) and 1 or 0\n\
             end\n\
             function update(node, dt) end\n",
        );
        let (mut world, e) = world_with_script("bake");
        let mut host = ScriptHost::new();
        host.set_anim_info(HashMap::from([(
            e.index(),
            AnimInfo {
                layers: vec![("Base".into(), None, 0.0, false)],
                clips: Rc::new(vec![ClipInfo {
                    name: "Punch".into(),
                    duration: 0.5,
                    events: vec![(0.125, "onHitboxStart".into()), (0.25, "onHitboxEnd".into())],
                }]),
            },
        )]));
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let tr = world.get::<Transform>(e).unwrap().translation;
        assert_eq!(tr.x, 3.0, "0.125s of a 0.5s clip over 12 frames is frame 3");
        assert_eq!(tr.y, 2.0, "both authored events came through");
        assert_eq!(tr.z, 1.0, "an unknown clip reads nil rather than erroring");
    }

    /// A bad table shape passed to a construction API is a script error in the Console,
    /// never a process abort. It used to take the whole editor down with SIGABRT
    /// ("panic in a function that cannot unwind"), losing unsaved work and telling the
    /// author nothing about what they got wrong (floptle/0025).
    #[test]
    fn a_bad_field_shape_is_a_script_error_not_an_abort() {
        let dir = std::env::temp_dir().join("floptle_script_test_bad_field_shape");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "flash",
            "function update(node, dt)\n\
               node:setMaterial{ emissive = { nope = 1 } }\n\
             end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Matter::Empty);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "flash".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.016, 0.0);
        let errs = host.errors();
        assert!(!errs.is_empty(), "the bad shape must surface as a script error");
        assert!(
            errs[0].contains("flash") && errs[0].contains("emissive"),
            "the error must name the script and the offending field: {errs:?}"
        );
    }

    /// The colour spellings the docs promise all reach the component. `{r,g,b}` was
    /// documented in `floptle.lua` and named in the converter's own error message, and
    /// was the one shape it refused.
    #[test]
    fn set_material_accepts_every_documented_colour_shape() {
        let dir = std::env::temp_dir().join("floptle_script_test_colour_shapes");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "paint",
            "function update(node, dt)\n\
               node:setMaterial{ color = { r = 1, g = 0.5, b = 0.25 } }\n\
               find(\"B\"):setMaterial{ color = { x = 1, y = 0.5, z = 0.25 } }\n\
               find(\"C\"):setMaterial{ color = { 1, 0.5, 0.25 } }\n\
               find(\"D\"):setMaterial{ color = vec3(1, 0.5, 0.25) }\n\
             end\n",
        );
        let mut world = World::default();
        let mut nodes = Vec::new();
        for name in ["A", "B", "C", "D"] {
            let e = world.spawn();
            world.insert(e, Transform::IDENTITY);
            world.insert(e, floptle_core::Name(name.into()));
            world.insert(e, Matter::Empty);
            nodes.push(e);
        }
        world.insert(
            nodes[0],
            Scripts(vec![floptle_core::ScriptInst {
                kind: "paint".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.016, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        for (e, name) in nodes.iter().zip(["{r,g,b}", "{x,y,z}", "array", "vec3"]) {
            let m = world.get::<floptle_core::Material>(*e).expect(name);
            assert!(
                (m.color[0] - 1.0).abs() < 1e-5
                    && (m.color[1] - 0.5).abs() < 1e-5
                    && (m.color[2] - 0.25).abs() < 1e-5,
                "{name} did not reach the material: {:?}",
                m.color
            );
        }
    }

    /// `me = node` kept from `start()` must read the CURRENT pose on later hooks.
    ///
    /// It used to freeze at the spawn position: `node_table` built a fresh table per
    /// hook with x/y/z as raw fields, so the stashed reference was a snapshot. It failed
    /// silently and only partially — everything using the PASSED `node` stayed correct,
    /// so a character moved and animated fine while anything derived from the stashed
    /// handle (hitboxes, hand-anchored effects) stayed nailed to the spawn point.
    #[test]
    fn a_handle_kept_from_start_tracks_the_node() {
        let dir = std::env::temp_dir().join("floptle_script_test_stashed_handle");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "walker",
            "function start(node) me = node end\n\
             function update(node, dt)\n\
               seen = me.x          -- read the STASHED handle, before this frame's move\n\
               node.x = node.x + 1\n\
             end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "walker".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        for i in 0..3 {
            host.run(&mut world, &dir, 0.016, i as f32 * 0.016);
        }
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 3.0, "the body walked");
        let seen = host
            .instance_env(e.index(), "walker")
            .and_then(|env| env.get::<f64>("seen").ok())
            .unwrap_or(f64::NAN);
        assert_eq!(seen, 2.0, "the stashed handle must track the body, not freeze at spawn");
    }

    /// The rollback contract: `snapshot()` captures, re-simulation mutates, and
    /// `restore(s)` puts it back — with the ENGINE owning the copy in both
    /// directions, so a replay that mutates its restored state cannot corrupt the
    /// snapshot it came from. That corruption is the failure mode that would only
    /// show up under packet loss, on the second replay of the same tick.
    #[test]
    fn snapshot_and_restore_round_trip_and_survive_re_simulation() {
        let dir = std::env::temp_dir().join("floptle_script_test_rollback_hooks");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "fighter",
            "hp = 100\n\
             combo = { hits = 0, tags = { \"a\" } }\n\
             function fixedUpdate(node, dt)\n\
               hp = hp - 1\n\
               combo.hits = combo.hits + 1\n\
               table.insert(combo.tags, \"x\")\n\
             end\n\
             function snapshot() return { hp = hp, combo = combo } end\n\
             function restore(s) hp = s.hp; combo = s.combo end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "fighter".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0); // start()
        assert!(host.has_rollback_hooks(e.index()), "the hooks must be visible to the driver");

        let read = |h: &ScriptHost| -> (f64, f64, usize) {
            let env = h.instance_env(e.index(), "fighter").unwrap();
            let combo: mlua::Table = env.get("combo").unwrap();
            (
                env.get::<f64>("hp").unwrap(),
                combo.get::<f64>("hits").unwrap(),
                combo.get::<mlua::Table>("tags").unwrap().raw_len(),
            )
        };

        // Confirmed tick, then three provisional ones.
        let saved = host.snapshot_scripts(e.index());
        for _ in 0..3 {
            host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
        }
        assert_eq!(read(&host), (97.0, 3.0, 4));

        // A correction arrives: restore and re-simulate the same three ticks.
        host.restore_scripts(e.index(), &saved);
        assert_eq!(read(&host), (100.0, 0.0, 1), "restored to the confirmed tick");
        for _ in 0..3 {
            host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
        }
        assert_eq!(read(&host), (97.0, 3.0, 4), "the replay reproduces the same result");

        // …and the SECOND replay off the same snapshot must too. It won't if the
        // capture shared its tables with the sim, because the first replay would
        // have mutated them.
        host.restore_scripts(e.index(), &saved);
        assert_eq!(read(&host), (100.0, 0.0, 1), "the snapshot is still pristine");
        for _ in 0..3 {
            host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
        }
        assert_eq!(read(&host), (97.0, 3.0, 4));
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    }

    /// A script with no hooks is not rolled back and must not be an error — that
    /// is the documented default for cosmetics. And a snapshot holding something
    /// unrestorable is refused loudly rather than silently dropped, because a
    /// state that looks restored and isn't is the worst of both.
    #[test]
    fn scripts_without_hooks_are_skipped_and_bad_state_is_refused() {
        let dir = std::env::temp_dir().join("floptle_script_test_rollback_optout");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "cosmetic", "function fixedUpdate(node, dt) end\n");
        write_script(
            &dir,
            "broken",
            "function snapshot() return { cb = function() end } end\n\
             function restore(s) end\n\
             function fixedUpdate(node, dt) end\n",
        );
        let mut world = World::default();
        let plain = world.spawn();
        world.insert(plain, Transform::IDENTITY);
        world.insert(
            plain,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "cosmetic".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let bad = world.spawn();
        world.insert(bad, Transform::IDENTITY);
        world.insert(
            bad,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "broken".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);

        assert!(!host.has_rollback_hooks(plain.index()));
        let s = host.snapshot_scripts(plain.index());
        assert!(s.is_empty(), "no hooks, nothing captured");
        host.restore_scripts(plain.index(), &s); // and restoring is a no-op
        assert!(host.errors().is_empty(), "opting out is not an error: {:?}", host.errors());

        let s = host.snapshot_scripts(bad.index());
        assert!(s.is_empty(), "the unrestorable capture is refused, not stored");
        let errs = host.errors();
        assert!(
            errs.iter().any(|e| e.contains("broken") && e.contains("rolled back")),
            "the error must name the script and say what's wrong: {errs:?}"
        );
    }

    /// The tick-pose channel (`docs/rollback-netcode-design.md` §3).
    ///
    /// `node.x` between ticks is the INTERPOLATED render pose — lerped by the
    /// frame's alpha, so reading it inside `fixedUpdate` is a frame-rate-
    /// dependent read that no replay can reproduce, and `node.x = node.x + d`
    /// teleports the body onto its visual position (the classic "the visuals
    /// take the knockback but the hitbox stays put" bug). `node.tickX/tickY/
    /// tickZ/tickPos` are the body's own pose, and writing them moves the body
    /// without going near the transform.
    #[test]
    fn the_tick_pose_channel_reads_and_writes_the_body_not_the_render_transform() {
        let dir = std::env::temp_dir().join("floptle_script_test_tick_pose");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "fighter",
            "function fixedUpdate(node, dt)\n\
               sawX, sawRender = node.tickX, node.x\n\
               sawPos = node.tickPos.y\n\
               if node.tickX < 100 then node.tickX = node.tickX + 5 end\n\
             end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        // The render transform is deliberately somewhere the body is NOT — that
        // is exactly the situation mid-tick, and the two must not be confused.
        world.insert(e, Transform::from_translation(glam::DVec3::new(-99.0, 0.0, 0.0)));
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "fighter".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.set_bodies(HashMap::from([(
            e.index(),
            BodyState { pos: [10.0, 3.0, -2.0], ..Default::default() },
        )]));
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        host.run_fixed(&mut world, 1.0 / 60.0, 0.0);

        let env = host.instance_env(e.index(), "fighter").unwrap();
        assert_eq!(env.get::<f64>("sawX").unwrap(), 10.0, "tickX is the BODY's pose");
        assert_eq!(env.get::<f64>("sawRender").unwrap(), -99.0, "…and node.x is not");
        assert_eq!(env.get::<f64>("sawPos").unwrap(), 3.0, "tickPos is the same pose as a vec3");

        // The write became a body teleport, not a transform edit.
        let moved = host.take_body_pos_changes();
        assert_eq!(moved.get(&e.index()).copied(), Some([15.0, 3.0, -2.0]));
        assert_eq!(
            world.get::<Transform>(e).unwrap().translation.x,
            -99.0,
            "the render transform must be left exactly alone"
        );
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    }

    /// `net.random()` (`docs/rollback-netcode-design.md` §3): identical on
    /// every peer for a tick, identical again when that tick is re-simulated.
    ///
    /// The second half is the one a hand-rolled `rng(matchSeed + tick)` gets
    /// wrong — it re-seeds per tick but not per *draw*, so two calls in one tick
    /// return the same number, and authors work around that by adding state
    /// that then has to be rolled back too.
    #[test]
    fn net_random_is_identical_per_tick_and_across_a_replay() {
        let dir = std::env::temp_dir().join("floptle_script_test_net_random");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "roller",
            "rolls = {}\n\
             function fixedUpdate(node, dt)\n\
               rolls[#rolls + 1] = net.random()\n\
               rolls[#rolls + 1] = net.random()\n\
               rolls[#rolls + 1] = net.random(1, 6)\n\
             end\n\
             function snapshot() return { n = #rolls } end\n\
             function restore(s) while #rolls > s.n do table.remove(rolls) end end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "roller".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        let info = |tick: u64| RollbackInfo {
            active: true,
            tick,
            seed: 0x0BAD_F00D_1234_5678,
            ..Default::default()
        };
        let read = |h: &ScriptHost| -> Vec<f64> {
            h.instance_env(e.index(), "roller")
                .unwrap()
                .get::<mlua::Table>("rolls")
                .unwrap()
                .sequence_values::<f64>()
                .flatten()
                .collect()
        };

        for tick in 1..=3u64 {
            host.set_rollback_info(info(tick));
            host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
        }
        let live = read(&host);
        assert_eq!(live.len(), 9);
        assert_ne!(live[0], live[1], "two draws in one tick must differ");
        assert_ne!(live[0], live[3], "and two ticks must differ");
        assert!(live.iter().take(2).all(|v| (0.0..1.0).contains(v)), "unit range");
        assert!(live[2] >= 1.0 && live[2] <= 6.0 && live[2].fract() == 0.0, "a d6: {}", live[2]);

        // Re-simulate ticks 2..=3 — as a correction would. The script keeps
        // appending, so the replay's draws land after the live ones and the two
        // stretches can be compared directly.
        for tick in 2..=3u64 {
            host.set_rollback_info(info(tick));
            host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
        }
        let replayed = read(&host);
        assert_eq!(&replayed[9..], &live[3..], "a replayed tick must roll the same numbers");
    }

    /// A node with no rigidbody has no tick channel, and saying so beats a
    /// silent no-op that looks like a working teleport.
    #[test]
    fn the_tick_pose_channel_is_absent_without_a_body() {
        let dir = std::env::temp_dir().join("floptle_script_test_tick_pose_nobody");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "prop",
            "function fixedUpdate(node, dt)\n\
               missing = (node.tickPos == nil) and (node.tickX == nil)\n\
               refused = not pcall(function() node.tickX = 5 end)\n\
             end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "prop".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
        let env = host.instance_env(e.index(), "prop").unwrap();
        assert!(env.get::<bool>("missing").unwrap(), "no body, no tick pose");
        assert!(env.get::<bool>("refused").unwrap(), "and writing it is an error, not a no-op");
    }

    /// The `replaying` gate (`docs/rollback-netcode-design.md` §4): a
    /// re-simulated tick runs the same Lua the live tick ran, so its one-shot
    /// cosmetics must not fire a second time — while everything the simulation
    /// depends on still lands, and a raised error still reaches the Console.
    #[test]
    fn a_replay_suppresses_one_shot_side_effects_but_not_simulation_writes() {
        let dir = std::env::temp_dir().join("floptle_script_test_replay_gate");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "noisy",
            "hits = 0\n\
             function fixedUpdate(node, dt)\n\
               hits = hits + 1\n\
               node.vx = hits\n\
               print(\"hit \" .. hits)\n\
               spawnEffect(\"spark\", 1, 2, 3)\n\
               audio.play(\"thud\")\n\
               spawn(\"fireball\", vec3(0, 0, 0))\n\
               net.rpc(\"scored\", { n = hits })\n\
             end\n\
             function snapshot() return { hits = hits } end\n\
             function restore(s) hits = s.hits end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "noisy".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        let saved = host.snapshot_scripts(e.index());

        // Two LIVE ticks: every queue fills as usual.
        for _ in 0..2 {
            host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
        }
        assert_eq!(host.take_spawn_effects().len(), 2);
        assert_eq!(host.take_audio_commands().len(), 2);
        assert_eq!(host.take_spawn_requests().len(), 2);
        assert_eq!(host.take_net_commands().len(), 2);
        assert_eq!(host.drain_logs().len(), 2);
        assert_eq!(host.take_body_changes().get(&e.index()).map(|v| v[0]), Some(2.0));

        // A correction: the SAME two ticks re-simulate under the gate.
        host.restore_scripts(e.index(), &saved);
        host.begin_replay();
        assert!(host.is_replaying());
        for _ in 0..2 {
            host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
        }
        host.end_replay();
        assert!(!host.is_replaying());

        assert!(host.take_spawn_effects().is_empty(), "the hit spark must not double");
        assert!(host.take_audio_commands().is_empty(), "nor the impact stutter");
        assert!(host.take_spawn_requests().is_empty(), "nor the projectile duplicate");
        assert!(host.take_net_commands().is_empty(), "nor the rpc send twice");
        assert!(host.drain_logs().is_empty(), "nor the Console flood");
        // …while the simulation write the replay exists to produce still lands.
        assert_eq!(
            host.take_body_changes().get(&e.index()).map(|v| v[0]),
            Some(2.0),
            "body writes are the POINT of the replay and must survive the gate"
        );
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    }

    /// A replay that throws is a correctness problem, not noise: suppressing it
    /// would leave a desync with no symptom at all.
    #[test]
    fn a_replay_never_suppresses_an_error() {
        let dir = std::env::temp_dir().join("floptle_script_test_replay_errors");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "thrower",
            "function fixedUpdate(node, dt) print(\"quiet\"); error(\"boom\") end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "thrower".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        host.begin_replay();
        host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
        host.end_replay();
        let logs = host.drain_logs();
        assert!(
            logs.iter().any(|l| l.level == LogLevel::Error && l.msg.contains("boom")),
            "the raised error must survive the gate: {logs:?}"
        );
        assert!(!logs.iter().any(|l| l.msg.contains("quiet")), "…but the print must not");
    }

    /// Physics moving a body between hooks must NOT read as a pending write through the
    /// stashed handle — otherwise every tick would teleport the body back to where the
    /// table happened to be left. The drain compares the table against what the engine
    /// last stamped INTO it, not against the transform.
    #[test]
    fn physics_moving_a_body_is_not_mistaken_for_a_stashed_write() {
        let dir = std::env::temp_dir().join("floptle_script_test_no_phantom_teleport");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "rider",
            "function start(node) me = node end\n\
             function update(node, dt) seen = me.x end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "rider".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.016, 0.0);
        // The driver (physics) moves the body between hooks; the script wrote nothing.
        for i in 1..=3 {
            world.get_mut::<Transform>(e).unwrap().translation.x = i as f64;
            host.run(&mut world, &dir, 0.016, i as f32 * 0.016);
            assert_eq!(
                world.get::<Transform>(e).unwrap().translation.x,
                i as f64,
                "the engine must not drag the body back to the table's last value"
            );
        }
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let seen = host
            .instance_env(e.index(), "rider")
            .and_then(|env| env.get::<f64>("seen").ok())
            .unwrap_or(f64::NAN);
        assert_eq!(seen, 3.0, "and the stashed handle still reads the live pose");
    }

    /// A write through a stashed handle from OUTSIDE that script's hooks — the shape a
    /// cross-script `other:knockBack()` takes — lands. It used to be dropped: the write
    /// arrived after the target's read-back had drained, and the next hook's re-stamp
    /// overwrote it.
    #[test]
    fn a_cross_script_write_through_a_stashed_handle_lands() {
        let dir = std::env::temp_dir().join("floptle_script_test_cross_script_write");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "target",
            "function start(node) me = node end\n\
             function teleport(x) me.x = x end\n\
             function update(node, dt) end\n",
        );
        write_script(
            &dir,
            "caller",
            "function update(node, dt)\n\
               if not done then\n\
                 find(\"Target\"):getscript(\"target\").teleport(-5)\n\
                 done = true\n\
               end\n\
             end\n",
        );
        let mut world = World::default();
        let target = world.spawn();
        world.insert(target, Transform::IDENTITY);
        world.insert(target, floptle_core::Name("Target".into()));
        world.insert(
            target,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "target".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let caller = world.spawn();
        world.insert(caller, Transform::IDENTITY);
        world.insert(caller, floptle_core::Name("Caller".into()));
        world.insert(
            caller,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "caller".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        for i in 0..4 {
            host.run(&mut world, &dir, 0.016, i as f32 * 0.016);
        }
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(
            world.get::<Transform>(target).unwrap().translation.x,
            -5.0,
            "the teleport written through the stashed handle must reach the transform"
        );
    }
}
