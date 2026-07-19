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

/// One world-space line segment a script queued via `draw.line(...)` this tick
/// (immediate mode — re-queued every tick while wanted). Drawn depth-tested by
/// the runtime line layer; the S6 v2 map draws its orbit conics with these.
#[derive(Clone, Copy, Debug)]
pub struct DrawLine {
    pub a: [f64; 3],
    pub b: [f64; 3],
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

mod api;
mod audio_api;
mod env;
mod host;
mod math_api;
mod net_api;
mod preprocess;
mod save_api;
mod sched_api;
mod space_api;
mod terrain_api;
mod view_api;

pub(crate) use api::install_handle_api;
/// Live ECS field appliers, reused by the animation system's property tracks.
/// `mirror_components` reads them back (numeric) — the animation recorder diffs
/// it to auto-key changed properties.
pub use api::{apply_component_field, apply_component_field_str, mirror_components};
pub use net_api::{input_to_net, net_to_input, NetCmd, NetRoleState, NetState, RewindScope};
pub use space_api::{SpaceBodyInfo, SpaceInfo};
pub use terrain_api::{TerrainOp, TerrainOpMode};
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
    /// This frame's physics body state per entity index (velocity + grounded), fed in
    /// before `run` so scripts can read `node.vx/vy/vz/grounded`.
    bodies: Rc<RefCell<HashMap<u32, BodyState>>>,
    /// Velocities scripts wrote this frame (entity index → new velocity), drained by
    /// the editor and applied to the physics sim.
    body_changes: Rc<RefCell<HashMap<u32, [f32; 3]>>>,
    /// Capsule heights scripts wrote this frame (entity index → height), drained and
    /// applied to the sim — for crouching.
    body_height_changes: Rc<RefCell<HashMap<u32, f32>>>,
    /// Cross-node position writes on body entities → the driver teleports the
    /// body (see `Shared::body_pos_changes`).
    body_pos_changes: Rc<RefCell<HashMap<u32, [f64; 3]>>>,
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
    /// `terrain.generatePlanet(id, opts)` requests — heavyweight whole-field
    /// generations the editor runs on a background thread.
    terrain_generates: Rc<RefCell<Vec<(u32, floptle_field::procgen::PlanetFill)>>>,
    /// `createNode(...)` requests, drained with the spawn queue.
    create_requests: Rc<RefCell<Vec<CreateRequest>>>,
    /// Construction-API component/matter writes (see [`RichSet`]).
    rich_sets: Rc<RefCell<Vec<(u32, RichSet)>>>,
    /// The scene graph mirror the node handles read/write (synced each `run`).
    scene: Rc<RefCell<SceneMirror>>,
    /// Live per-(entity, script) environments, for script handles.
    envs: Rc<RefCell<HashMap<(u32, String), Table>>>,
    /// Mesh model paths scripts wrote this frame (entity index → new asset path), applied
    /// to the ECS `Matter::Mesh` in `run` and drained by the editor to re-import the GPU mesh.
    model_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// Material refs scripts assigned this frame (entity index → preset name / asset path),
    /// resolved against `materials` and applied to the ECS in `run`.
    material_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// `node.visible = ...` writes (entity index → shown), applied as a `Visible` component.
    visible_changes: Rc<RefCell<HashMap<u32, bool>>>,
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
    /// `node:getcomponent(name).field = value` writes, flushed to the ECS after `run`.
    component_changes: ComponentWrites,
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
    /// A pending mouse-lock request from `input.lockMouse()` / `input.unlockMouse()`:
    /// `Some(true)` = lock (grab + hide the cursor), `Some(false)` = unlock, `None` = no
    /// change this frame. The editor drains it after `run` and applies it to the window.
    mouse_lock: Rc<RefCell<Option<bool>>>,
    /// `params.X = value` writes queued this pass — (entity, script kind, key,
    /// value). Flushed to the node's stored `ScriptInst` params so tunables are
    /// TWO-WAY: the write persists across frames and shows live in the
    /// Inspector (and reverts on Stop like every play-mode change). Numbers
    /// AND strings; only DECLARED tunables persist (a key in `defaults` or the
    /// stored params).
    param_writes: RefCell<Vec<(u32, String, String, ParamWrite)>>,
    /// A pending `scene.load(...)` request (last call this frame wins). The
    /// driver drains it and performs the switch between frames — locally when
    /// offline/hosting, over the wire to every client in a session.
    scene_request: Rc<RefCell<Option<String>>>,
    /// The running scene's name, fed by the driver — what `scene.current()` reads.
    scene_name: Rc<RefCell<String>>,
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
    /// Entities whose scripts are SKIPPED this session (a networked CLIENT
    /// doesn't run server-authoritative nodes' scripts — their state arrives
    /// in snapshots; docs/netcode-design.md §6). Set by the driver.
    script_skip: std::collections::HashSet<u32>,
    /// Entities skipped in the PER-FRAME pass only: a predicted node's
    /// `update` re-runs on the gameplay tick (`run_frame_for`) so client and
    /// server integrate identically.
    frame_skip: std::collections::HashSet<u32>,
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
    /// Every playable state name across all layers.
    pub states: Vec<String>,
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
pub type SpawnedEffect = (String, [f64; 3]);

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
#[derive(Default)]
struct SceneMirror {
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
    /// Live transforms (read/written by node handles; flushed to the ECS after `run`).
    transforms: HashMap<u32, Transform>,
    /// Mesh nodes' current model path (so a script can read `node.model`).
    models: HashMap<u32, String>,
    /// UI elements' current text (so a script can read `node.text`).
    ui_texts: HashMap<u32, String>,
    /// Nodes that carry an explicit `Visible` component (so a script can read
    /// `node.visible`; absent = visible by default).
    visible: HashMap<u32, bool>,
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
    MatterPrimitive(String, [f64; 3]),
}

/// The interior-mutable state the Lua handle closures share with the host: the scene
/// mirror, the physics body bridges, and the per-(entity, script) environments.
#[derive(Clone)]
struct Shared {
    scene: Rc<RefCell<SceneMirror>>,
    bodies: Rc<RefCell<HashMap<u32, BodyState>>>,
    body_changes: Rc<RefCell<HashMap<u32, [f32; 3]>>>,
    body_height_changes: Rc<RefCell<HashMap<u32, f32>>>,
    /// Cross-node POSITION writes onto entities that HAVE a physics body —
    /// the driver TELEPORTS the body there (otherwise the physics writeback
    /// stomps the transform next frame and the write silently vanishes).
    body_pos_changes: Rc<RefCell<HashMap<u32, [f64; 3]>>>,
    /// `node:setShaderParam(...)` writes, drained by the editor per frame.
    shader_param_sets: ShaderParamSets,
    /// (entity index, script kind) → that instance's live Lua environment table, so a
    /// script handle can read its state, call its methods, and read its params.
    envs: Rc<RefCell<HashMap<(u32, String), Table>>>,
    /// `node.model = ...` writes (entity index → asset path), applied to `Matter::Mesh`.
    model_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// `node.material = ...` writes (entity index → preset name / asset path).
    material_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// `node.visible = ...` writes (entity index → shown), applied as a `Visible` component.
    visible_changes: Rc<RefCell<HashMap<u32, bool>>>,
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
    /// `node:getcomponent(name).field = value` writes: (entity, component, field) → number,
    /// flushed to the ECS after `run` (and read back the same frame).
    component_changes: ComponentWrites,
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
}

impl Default for BodyState {
    fn default() -> Self {
        Self { vel: [0.0; 3], up: [0.0, 1.0, 0.0], grounded: false, height: 2.0 }
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
  createNode("Child", node, function(c) c:setTerrain(3) end)
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
        host.set_net_state(NetState { role: NetRoleState::Client, peers: vec![], rtt_ms: 0.0, my_peer: Some(7) });
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
            BodyState { vel: [0.0; 3], up: [0.0, 1.0, 0.0], grounded: true, height: 2.0 },
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
        let hp: f64 = {
            let key = (dummy.index(), "health".to_string());
            let env = host.envs.borrow().get(&key).cloned().unwrap();
            env.get("hp").unwrap()
        };
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
             if rb.lock_y > 0 then rb.friction = -1 end -- read-back sanity: must be 0\n\
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
            BodyState { vel: [0.0, -2.0, 0.0], up: [0.0, 1.0, 0.0], grounded: true, height: 2.0 },
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
        host.set_net_state(NetState { role: NetRoleState::Server, peers: vec![7], rtt_ms: 0.0, my_peer: None });
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
}
