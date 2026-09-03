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
/// `node:setShaderTexture(slot, path)` writes, queued per frame: (entity, slot
/// name, texture ref). The ref is a project-relative image path, an `rt:` render
/// target, or the empty string to clear the slot.
type ShaderTextureSets = Rc<RefCell<Vec<(u32, String, String)>>>;
/// `node:setScreenShader(name, on)` toggles, queued per frame: (entity, the
/// screen shader's file stem, on). Its own queue rather than a magic uniform
/// name, because a shader is free to declare a knob called `enabled` and the
/// two must not mean the same thing.
type ScreenShaderToggles = Rc<RefCell<Vec<(u32, String, bool)>>>;

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
    /// Project-relative `.ttf`/`.otf` path, or empty for the project's own UI
    /// font (`floptle/0124`).
    ///
    /// Empty used to mean the embedded Roboto and nothing else, because project
    /// fonts append to the font stack and slot 0 was never theirs. A game whose
    /// UI is a pixel font could not draw one immediate-mode string in it — and
    /// the symptom is not "wrong typeface", it is text that reads as badly
    /// spaced, because a layout built on a monospace grid is being handed a
    /// proportional font.
    pub font: String,
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
/// DETERMINISM INVARIANT (audited 2026-07-06, `docs/multiplayer.md` §3): the
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

pub mod app_api;
mod account_api;
mod api;
mod audio_api;
mod env;
mod host;
mod http_api;
pub use http_api::open_in_browser;
mod input_api;
pub mod json_array;
pub mod load_error;
mod math_api;
/// The vector [`nav_api`] hands back.
///
/// Exported because [`nav_api::install_mesh_reads`] can be installed into a host
/// that is not this one — the editor's package environment is the other — and a
/// public function that returns a private type leaves that host unable to read
/// its own answers.
pub use math_api::{ExactVec3, LuaVec3, Vec3Mode};

/// Read a 3-vector out of any Lua value this engine treats as one: a `vec3` in
/// EITHER backing, a `vec2` (z = 0), a node handle, or a `{x=, y=, z=}` table.
///
/// **The public read path, and the reason it exists is a bug it now prevents.**
/// A `vec3` used to be exactly one Rust type in a userdata, so a caller outside
/// this crate could `borrow::<LuaVec3>()` and be right. With two backings that
/// is no longer true, and the failure is silent in the worst way:
/// `AnyUserData::borrow` is bounded on `'static` and not on `UserData`, so a
/// borrow of the wrong type still COMPILES and merely never matches. Ask here
/// instead.
pub fn vec3_of(v: &mlua::Value) -> Option<glam::DVec3> {
    math_api::vec3_of(v)
}

/// Choose a state's vec3 backing. Call before anything else populates it —
/// `fast` installs methods on the vector type's metatable, which is global to
/// the state.
pub fn set_vec3_mode(lua: &mlua::Lua, mode: Vec3Mode) -> mlua::Result<()> {
    math_api::set_mode_checked(lua, mode)
}
pub mod nav_api;
pub mod access_api;
mod net_api;
pub mod opts;
mod perf_api;
pub mod rollback_api;
pub mod runtime_error;
mod preprocess;
mod save_api;
mod scatter_api;
mod sched_api;
mod shape_api;
mod assembly_api;
mod space_api;
mod steam_api;
mod terrain_api;
pub mod ui_make;
pub mod vm;
mod view_api;
pub mod water_api;

pub(crate) use api::install_handle_api;
/// Live ECS field appliers, reused by the animation system's property tracks.
/// `mirror_components` reads them back (numeric) — the animation recorder diffs
/// it to auto-key changed properties.
pub use api::{
    apply_component_color, apply_component_field, apply_component_field_str, apply_sprite_frame,
    effective_cell, mirror_component_strings, read_sprite_frame, set_sprite_cell,
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
    /// Where the file is, so a runtime error can quote the line it names.
    ///
    /// The path rather than the text: an error is rare and a script is small,
    /// so reading one line when something goes wrong costs nothing, while
    /// keeping every script's source resident costs on every project. The one
    /// window this opens — a file edited between raising and reporting, inside
    /// a single frame — resolves itself, because a changed mtime bumps the
    /// generation and clears the cached error.
    path: PathBuf,
    /// The file's text, read the first time an error needs a line quoted and
    /// kept until the file changes (the same mtime bump that resets
    /// `generation` drops it). Only a script that has RAISED is resident: a
    /// script raising in `update` raises every frame, on every instance, and
    /// reading the file per error was one read per instance per pass.
    text: Option<std::rc::Rc<str>>,
    /// How many times the file has been read for a quote — what the guard on
    /// the above counts.
    reads: u32,
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
    /// Which lifecycle hooks this script's environment defines, read once when
    /// the chunk is built (and again on hot reload — a rebuild is a new
    /// `Instance`). A pass the script has no hook for skips everything but the
    /// node stamp: with three passes a frame, an instance that only defines
    /// `update` was otherwise paying the full setup twice more for nothing.
    hooks: Hooks,
    /// A fingerprint of the `(params, refs, strs)` last seeded into
    /// `env.params`, so the table is rebuilt when the seed changes rather than
    /// on every hook call. `0` means "never seeded" and forces a build.
    seed_fp: u64,
    /// Whether this script's SOURCE ever assigns into `params` (`params.x =`,
    /// `params["x"] =`, `params[k] =`). Read once when the chunk is built.
    /// A script that never writes cannot have written, so the per-hook scan of
    /// the whole `params` table — a `String` per key per call — is skipped for
    /// it. Most scripts never write: three of Forgery's fifty-one do.
    ///
    /// A textual test, and conservative in the right direction: anything that
    /// LOOKS like a write counts as one, and a script that reaches `params`
    /// through an alias (`local p = params; p.x = 1`) is caught by the
    /// `params` mention plus an assignment through it being impossible to rule
    /// out — see [`source_writes_params`].
    writes_params: bool,
    /// A script wrote into `params` during its last hook. The table is rebuilt
    /// from the seed on the next pass — the same reset the per-call rebuild
    /// always gave an undeclared, frame-local param — and declared params come
    /// back through the ECS write in `flush_writes`, as they always have.
    params_dirty: bool,
}

/// The lifecycle hooks a script's environment defines — see [`Instance::hooks`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Hooks {
    start: bool,
    update: bool,
    fixed: bool,
    late: bool,
}

impl Hooks {
    /// Read from a freshly built environment. Every spelling `tick` accepts is
    /// asked about here, so the answer agrees with what it would have called.
    fn of(env: &mlua::Table) -> Self {
        use crate::env::lifecycle_fn;
        let has = |names: &[&str]| lifecycle_fn(env, names).ok().flatten().is_some();
        Self {
            start: has(&["start", "on_start"]),
            update: has(&["update", "on_update"]),
            fixed: has(&["fixedUpdate", "onFixedUpdate"]),
            late: has(&["lateUpdate", "onLateUpdate"]),
        }
    }
}

/// Does this source text assign into `params`?
///
/// Conservative: a write through an alias cannot be seen textually, so a
/// script that binds `params` to a local (`= params`) or passes it along
/// (`(params`, `, params`) is treated as a writer. A false "writes" costs the
/// old per-hook scan; a false "does not" would lose a write silently, which is
/// the outcome this must not have.
fn source_writes_params(src: &str) -> bool {
    let mut from = 0;
    while let Some(i) = src[from..].find("params") {
        let at = from + i;
        from = at + "params".len();
        // Not part of a longer identifier on either side.
        if at > 0 && src.as_bytes()[at - 1].is_ascii_alphanumeric() {
            continue;
        }
        let rest = src[from..].trim_start();
        // `params.x = …` / `params["x"] = …` / `params[k] = …`
        if let Some(r) = rest.strip_prefix('.').or_else(|| rest.strip_prefix('[')) {
            let r = r.trim_start();
            // skip the key: an identifier, or anything up to the closing `]`
            let after_key = if rest.starts_with('[') {
                match r.find(']') {
                    Some(j) => r[j + 1..].trim_start(),
                    None => continue,
                }
            } else {
                r.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_').trim_start()
            };
            if after_key.starts_with('=') && !after_key.starts_with("==") {
                return true;
            }
            continue;
        }
        // Escaped by alias or call: cannot be sure, so assume a write.
        if rest.starts_with(',') || rest.starts_with(')') || rest.starts_with('}') {
            return true;
        }
        if at > 0 {
            let before = src[..at].trim_end();
            if before.ends_with('=') || before.ends_with('(') || before.ends_with(',') || before.ends_with('{') {
                return true;
            }
        }
    }
    false
}

/// Fingerprint the seed an instance's `params` table is built from.
///
/// `structure` is the scene's structural revision as of the last full mirror
/// sync — passed as `0` by a script with no reference params, and folded in
/// for one that has them, because a ref is resolved by NAME and has to follow
/// a target that appears or is renamed mid-play. `0` is reserved for "never
/// built", so a real hash of zero is nudged.
fn seed_fingerprint(
    params: &[(String, f32)],
    refs: &[(String, String)],
    strs: &[(String, String)],
    structure: u64,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    structure.hash(&mut h);
    for (k, v) in params {
        k.hash(&mut h);
        v.to_bits().hash(&mut h);
    }
    0xffu8.hash(&mut h);
    refs.hash(&mut h);
    0xfeu8.hash(&mut h);
    strs.hash(&mut h);
    h.finish().max(1)
}

/// Embeds Lua and runs the scripts attached to a world's nodes.
pub struct ScriptHost {
    lua: Lua,
    /// Extra folders a script name may resolve in, after the project's own
    /// `scripts/`: the script folders of the project's installed **packages**,
    /// in load order. Set by the editor when packages load.
    ///
    /// The project always wins, so installing a package can never change what
    /// an existing script name means.
    extra_script_dirs: Vec<std::path::PathBuf>,
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
    /// See [`ShaderTextureSets`]. Separate from the uniform queue because a
    /// texture write is a REBIND, not a buffer write — the two cost different
    /// things and the driver treats them differently.
    shader_texture_sets: ShaderTextureSets,
    screen_shader_toggles: ScreenShaderToggles,
    /// The physics colliders for THIS frame, so `raycast(...)` works inside a script. The
    /// editor lends the sim's colliders before running scripts and takes them back after.
    colliders: Rc<RefCell<Vec<floptle_physics::AnchoredCollider>>>,
    /// Raycastable dynamic-body hulls for this frame ([`Sim::body_hulls`] copies —
    /// players, crates), fed alongside the colliders so `raycast(...)` can hit
    /// bodies AND name the node it hit (`hit.node`). `net.rewind` re-poses these
    /// for lag-compensated combat queries (`docs/multiplayer.md` §7).
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
    /// The editor's answer to `terrain.busy()`: true while the background
    /// terrain worker has a field generating or streaming in. Published each
    /// frame so a game that builds its world ON DEMAND can wait its turn
    /// instead of queueing new worlds behind the ground someone stands on.
    terrain_busy: Rc<std::cell::Cell<bool>>,
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
    /// Script kinds that failed to LOAD — shared with the reference layer, which
    /// reads it to tell a broken script apart from a missing export. See the
    /// `Shared` copy (`floptle/0086`).
    broken: Rc<RefCell<std::collections::HashSet<String>>>,
    broken_read_warned: Rc<RefCell<std::collections::HashSet<(String, String)>>>,
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
    /// The scene's baked navmesh, if it has one — what `nav.*` answers from.
    nav_mesh: nav_api::NavShared,
    /// Every `nav.agent` in the scene. Stepped once per frame by [`ScriptHost::run`],
    /// after scripts have had their say, so an order given this frame is walked
    /// this frame.
    nav_agents: nav_api::AgentsShared,
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
    /// What the game currently IS — title, engine version, and the video
    /// settings a player can change. Pushed by the driver, read by `app.*`
    /// (`floptle/0175`).
    app_info: crate::app_api::SharedAppInfo,
    /// What `app.*` asked the driver to change or do this frame. Every one of
    /// them touches something only the driver owns — the swap chain, a GPU
    /// target, the event loop — so none can be done from inside a Lua call.
    app_requests: crate::app_api::SharedAppRequests,
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
    /// `nav.rebake(...)`, waiting for the editor to gather the geometry.
    nav_rebakes: Rc<RefCell<Vec<NavRebakeRequest>>>,
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
    /// The `steam.*` bridge's backend — `NullPlatform` unless a caller has
    /// explicitly decided this session IS the game and called
    /// [`ScriptHost::set_platform`] (see the Steam integration plan's
    /// "Where Steam activates").
    platform: steam_api::SharedPlatform,
    /// The `steam.*` bridge's own state: just the registered
    /// `onPersonaChanged` callback.
    steam_state: Rc<RefCell<steam_api::SteamState>>,
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
    /// `net.on` handlers, and the current-instance marker (docs/multiplayer.md §8).
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
    /// Bytes of Lua heap allocated inside each script kind's hook calls while
    /// `alloc_track` is on — see [`ScriptHost::track_alloc`].
    alloc_by_kind: RefCell<HashMap<String, u64>>,
    alloc_track: std::cell::Cell<bool>,
    /// `(script kind, key)` combos already reported as shadowing a `findScript`
    /// handle's own key (`floptle/0085`) — one line per script per session, not
    /// one per instance.
    handle_key_warned: std::collections::HashSet<(String, String)>,
    /// `(script kind, generation)` whose LOAD failure has already been put on the
    /// Console. A broken script is re-reported into `errors` every frame (the
    /// Scripting tab is a live list), but the Console line is once per version
    /// of the file — otherwise one unloadable script buries every other message
    /// in the feed at sixty lines a second (`floptle/0086`).
    load_failure_reported: std::collections::HashSet<(String, u64)>,
    /// `(script kind, generation)` already warned as *approaching* LuaJIT's
    /// upvalue ceiling. Same once-per-version rule, and it clears on edit — so
    /// the warning comes back the moment the file grows again.
    upvalue_warned: std::collections::HashSet<(String, u64)>,
    /// Entities whose scripts are SKIPPED this session (a networked CLIENT
    /// doesn't run server-authoritative nodes' scripts — their state arrives
    /// in snapshots; docs/multiplayer.md §6). Set by the driver.
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
    /// (`docs/multiplayer.md` §4). Scripts read it as
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
    /// Sprite nodes' own numbers, so `node:sprite()` can READ them.
    ///
    /// `setSprite` shipped write-only, the same gap `sorting` above had: a
    /// character that flips on a turn has to ask which way it is facing, and a
    /// value you cannot read is one every caller ends up shadowing in a local —
    /// which is then the second copy that goes stale.
    ///
    /// Written by the per-frame sync AND by every script-side write, so a read
    /// straight after an assignment answers with what was just assigned rather
    /// than with what the frame started as (the queue itself does not apply
    /// until after the pass).
    pub(crate) sprites: HashMap<u32, SpriteMirror>,
    /// What each node said about sorting, so `node:sorting()` can READ it.
    ///
    /// `setSorting` shipped without a getter, which makes the obvious pattern —
    /// nudge a node one in front of whatever it is standing next to — impossible
    /// to write: you cannot add one to a number you cannot ask for. Absent means
    /// the node carries no `Sorting`, which the getter answers as the default
    /// rather than as nil, because "Default layer, order 0" is the true answer
    /// and nil would make every caller write the same fallback.
    pub(crate) sorting: HashMap<u32, (String, i32, &'static str)>,
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
    /// …and the STRING-valued half (`mat.texture`, `el.text`, `el.style`).
    ///
    /// Writable since strings landed, and readable by nothing: `mat.texture`
    /// answered nil however many times it had been set, so a script could not
    /// ask what a material was wearing — only tell it. Which makes the obvious
    /// swap ("put the shirt on unless it is already on") impossible to write.
    component_strings: HashMap<u32, HashMap<String, HashMap<String, String>>>,
    /// Model asset path → the material slots it was imported with, LENT by the
    /// editor (`ScriptHost::set_model_slots`) the way the tilesets are: the host
    /// does no file I/O, and a `.glb`'s parts are the importer's knowledge.
    ///
    /// This is what `node:materials()` answers from, and without it a script
    /// cannot even find out that a character's torso is called `Torso#2` —
    /// which is the one thing standing between a dev and a clothing system.
    pub(crate) model_slots: HashMap<String, Vec<ModelSlot>>,
    /// Entity index → its `Entity` (with generation), so handle-written transforms flush
    /// back to the right ECS entity.
    ents: HashMap<u32, Entity>,
    /// Entities whose transform a handle wrote this frame (so we only flush those back —
    /// the current node still flushes via the value-table path).
    dirty: std::collections::HashSet<u32>,
    /// `world.revision() - world.revision_of::<Transform>()` as of the last
    /// FULL sync — see `ScriptHost::sync_scene`. `0` means never synced.
    synced_non_transform_rev: u64,
}

/// Whether a `find*` call may return switched-off nodes.
///
/// Enabled-only is the DEFAULT, and it is the whole point: a node you switched
/// off in the Hierarchy is one you have decided is not part of the scene right
/// now. Its scripts do not run, physics skips it, it does not draw — but every
/// `find` in the engine handed it back anyway, so an old camera and an old
/// player kept being adopted by scripts that had no way to know they were
/// looking at a corpse. "Off" has to mean off in the place that does the looking.
///
/// The escape hatch stays, because a disabled node is a legitimate template: a
/// parked prefab you clone, a spare rig, a menu you turn on later.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FindScope {
    /// Skip anything switched off, itself or by an ancestor. The default.
    #[default]
    Enabled,
    /// Everything, switched off or not — the pre-0.42 behaviour, asked for.
    All,
    /// ONLY switched-off nodes — for a tool that manages the parked ones.
    Disabled,
}

impl FindScope {
    /// Every spelling the options table accepts, and the list an error prints.
    ///
    /// One list read by the parser AND the message, per `floptle/0082` — a
    /// defaulted bad value is how `pin = "topCenter"` silently meant top-left.
    pub(crate) const ACCEPTS: &'static [&'static str] = &["enabled", "all", "disabled", "any"];

    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "enabled" => Some(FindScope::Enabled),
            "all" | "any" => Some(FindScope::All),
            "disabled" => Some(FindScope::Disabled),
            _ => None,
        }
    }
}

impl SceneMirror {
    /// Is this node switched off — itself, or because an ancestor is?
    ///
    /// The mirror stores only each node's OWN `Disabled`, deliberately (the
    /// engine resolves inheritance and duplicating it would give two answers
    /// that can drift). So the walk happens here, bounded like every other
    /// parent walk in the engine, and only for candidates a lookup already
    /// matched — never per node per frame.
    pub(crate) fn off(&self, id: u32) -> bool {
        let mut cur = id;
        for _ in 0..64 {
            if self.disabled.contains(&cur) {
                return true;
            }
            match self.parent.get(&cur) {
                Some(&p) => cur = p,
                None => return false,
            }
        }
        false
    }

    /// Does `id` belong in the results a `scope` asked for?
    pub(crate) fn in_scope(&self, id: u32, scope: FindScope) -> bool {
        match scope {
            FindScope::All => true,
            FindScope::Enabled => !self.off(id),
            FindScope::Disabled => self.off(id),
        }
    }

    /// The script kinds attached to a node, in the order they were attached.
    pub(crate) fn kinds_on(&self, id: u32) -> &[String] {
        self.scripts.get(&id).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

/// The part of a script kind after the last `/` — the name the editor puts on
/// the tab, the Inspector row and the Hierarchy.
///
/// A script's `kind` is its path under `scripts/` without the extension, so a
/// file the author filed in a folder is `"forgery/playermovement"`. Nothing on
/// screen ever says that: the tab says `playermovement.lua`, the Console
/// attributes its output to `playermovement`, and every example in the docs
/// passes a bare name. See [`match_kind`].
pub(crate) fn kind_stem(kind: &str) -> &str {
    kind.rsplit('/').next().unwrap_or(kind)
}

/// How a script name asked for in Lua matched the kinds actually in play.
pub(crate) enum KindMatch {
    /// Exactly one kind answers to that name — the canonical kind to use.
    One(String),
    /// Nothing does.
    None,
    /// The bare stem fits several kinds. Refused rather than guessed: whichever
    /// one got picked would be a coin flip, and the fix — say which folder — is
    /// one word.
    Ambiguous(Vec<String>),
}

/// Match a name a script asked for against the kinds available.
///
/// An exact kind wins outright, so a project that already spells them in full
/// keeps the meaning it had. Otherwise the trailing [`kind_stem`] answers, which
/// is what makes `node:getscript("playermovement")` reach
/// `scripts/forgery/playermovement.lua` — the name the author sees everywhere
/// they look, and the one they type first.
///
/// This is the lookup behind `node:getscript`, `node:getcomponent`'s sibling
/// `findScript`/`findScripts`, and the `scriptref(...)` param binding, so all of
/// them agree on what a script is called.
pub(crate) fn match_kind<'a>(kinds: impl IntoIterator<Item = &'a str>, name: &str) -> KindMatch {
    let mut stem_hits: Vec<String> = Vec::new();
    for k in kinds {
        if k == name {
            return KindMatch::One(k.to_string());
        }
        if kind_stem(k) == name && !stem_hits.iter().any(|h| h == k) {
            stem_hits.push(k.to_string());
        }
    }
    match stem_hits.len() {
        0 => KindMatch::None,
        1 => KindMatch::One(stem_hits.swap_remove(0)),
        _ => KindMatch::Ambiguous(stem_hits),
    }
}

/// The sentence an ambiguous script name is refused with.
pub(crate) fn ambiguous_kind_error(call: &str, name: &str, hits: &[String]) -> mlua::Error {
    mlua::Error::runtime(format!(
        "{call}: \"{name}\" could mean {} — say which one, since a script's name is its \
         path under scripts/ without the .lua.",
        hits.join(" or ")
    ))
}

/// A prefab instance a script requested via `spawn(prefab [, pos [, fn]])`:
/// the prefab name/path, an optional world position for its first root, and
/// an optional callback (a Lua registry key) the driver invokes with the new
/// root's node handle once it exists (`ScriptHost::call_spawn_callback`).
/// A `nav.rebake(centre, size)` request: re-measure this box of the level and
/// splice the answer into the navmesh in hand.
///
/// A request rather than a call, like `spawn`, because it needs the world's
/// triangles and the scripting host does not have them — only the editor can
/// gather geometry, import models and voxelise. Drained in the same pass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NavRebakeRequest {
    /// The middle of the box, in world coordinates.
    pub centre: [f64; 3],
    /// How big it is. The bake snaps it outward to whole navmesh cells.
    pub size: [f32; 3],
}

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
    /// `node:setSorting{ layer =, order = }` — where a 2D node draws in the
    /// stack (`floptle/0109`).
    ///
    /// Sorting layers shipped in v0.37.0 with no way for a script to touch
    /// them, which makes the ordinary 2D moves impossible: a character stepping
    /// behind a counter, a card lifting above the hand, a pickup that must draw
    /// over the tiles it lands on.
    MatterSorting { layer: Option<String>, order: Option<i32>, mode: Option<String> },
    /// `node:setParallax{ x =, y = }` — the per-axis scroll factor.
    MatterParallax { x: Option<f32>, y: Option<f32> },
    /// `node:setCamera2D{ follow =, smoothing =, deadZoneX =, … }` — how an
    /// orthographic camera follows.
    ///
    /// Settable from a script because the target is: a camera follows the
    /// player, and which node that is may be spawned, chosen at a character
    /// select, or handed over mid-level. `follow = ""` stops following without
    /// throwing away the dead zone and limits set beside it.
    /// Every axis is its OWN option. Collapsing a pair into `[x, y]` at the
    /// binding — with `0.0` for the axis nobody mentioned — is how
    /// `setCamera2D{ maxY = 80 }` used to set `maxX` to zero and park the
    /// camera against a limit nobody wrote.
    MatterCamera2D {
        follow: Option<String>,
        smoothing: Option<f32>,
        dead_zone_x: Option<f32>,
        dead_zone_y: Option<f32>,
        limits_on: Option<bool>,
        min_x: Option<f32>,
        min_y: Option<f32>,
        max_x: Option<f32>,
        max_y: Option<f32>,
        /// Pixels per world unit to land the drawn camera on; `0` turns it off.
        pixel_snap: Option<f32>,
        /// `off = true` removes the behaviour entirely.
        off: bool,
    },
    /// `node:shake(amount, seconds)` — a screen shake on a 2D camera.
    CameraShake { amount: f32, seconds: f32 },
    /// `node:setSprite{ ppu =, size =, cell =, flipX =, flipY =, pivot = }` —
    /// make this node one sprite, or retune one.
    MatterSprite {
        ppu: Option<f32>,
        size: Option<f32>,
        cell: Option<u32>,
        flip_x: Option<bool>,
        flip_y: Option<bool>,
        /// Per axis, for the same reason the camera's pairs are: `setSprite{
        /// pivotY = 0 }` used to put `pivotX` back to 0.5, and that call is the
        /// documented way to move a character's origin to its feet.
        pivot_x: Option<f32>,
        pivot_y: Option<f32>,
    },
    /// `node:setTint(color [, alpha])` — a multiplier over everything this node
    /// draws, or `node:setTint()` to clear it.
    ///
    /// Separate from `Material` because it is a different act: a Material says
    /// what a thing is MADE OF and replaces the model's own materials, while a
    /// tint leaves all of that alone and multiplies over the result. Flashing a
    /// character red must not cost it its textures.
    NodeTint { color: [f32; 3], alpha: f32, clear: bool },
    /// `node:setLighting2D{ mode =, layers =, blocks = }` — the 2D lighting flag,
    /// the layers a light reaches, and whether this node blocks light
    /// (`floptle/0113`).
    ///
    /// One call rather than three because they are one feature and a node uses
    /// one half of it: a LIGHT sets `mode` and `layers`, a RECEIVER sets `mode`
    /// and `blocks`.
    MatterLighting2D {
        mode: Option<floptle_core::Lit2D>,
        layers: Option<Vec<String>>,
        blocks: Option<floptle_core::Cast2D>,
        /// The shaping half (`floptle/0126`, `0125`): full brightness out to
        /// `inner`, the exponent of the ramp after it, and whether casters stop
        /// this light at all.
        inner: Option<f32>,
        falloff: Option<f32>,
        shadows: Option<bool>,
    },
    /// `node:setPointLight{ color =, intensity =, range = }` (`floptle/0116`).
    ///
    /// Until this a script could WRITE an existing light's fields but never make
    /// one, so the only way to have dynamic light was to author N of them into
    /// the scene and pool them — which is also how a game exhausted the
    /// sixteen-slot budget with lights that were switched off. Every field is
    /// optional and keeps what the node already had, so this is a create AND an
    /// edit, like every other `set*` here.
    MatterPointLight {
        color: Option<[f32; 3]>,
        intensity: Option<f32>,
        range: Option<f32>,
    },
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

/// One material slot of an imported model: which sub-object, which material,
/// and whether the model brought a texture for it.
///
/// Both names are here because a part answers to both, and neither is
/// sufficient on its own: the OBJECT name addresses exactly one part but is
/// rewritten by import when a model repeats a name (`Torso` becomes `Torso#2`),
/// while the MATERIAL name is the one on the model's own materials list and
/// usually covers the group somebody means — a character's `Clothing` is its
/// torso and both arms, which is exactly what a clothing system wants to change
/// at once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelSlot {
    /// The sub-object this part belongs to — the override key that addresses
    /// this part and no other.
    pub object: String,
    /// The glTF material name — the key that addresses every part wearing it.
    pub material: String,
    /// Did the model arrive with a texture on this material? A script that is
    /// about to override it can tell whether it is replacing a picture or a
    /// flat colour.
    pub textured: bool,
}

/// One sprite node's drawing numbers, as `node:sprite()` reads them.
///
/// A copy rather than a borrow for the reason every mirror entry is one: a Lua
/// closure answers reads while the host holds no `&World`. Six numbers per
/// sprite node — nothing worth a change-detection dance.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SpriteMirror {
    pub(crate) ppu: f32,
    pub(crate) size: f32,
    pub(crate) cell: u32,
    pub(crate) flip_x: bool,
    pub(crate) flip_y: bool,
    pub(crate) pivot: [f32; 2],
}

impl Default for SpriteMirror {
    /// The same defaults the queued write falls back to when a node is only
    /// becoming a sprite now, so the two cannot disagree about what an
    /// unmentioned field starts as.
    fn default() -> Self {
        Self { ppu: 32.0, size: 1.0, cell: 0, flip_x: false, flip_y: false, pivot: [0.5, 0.5] }
    }
}

impl SpriteMirror {
    /// These numbers, read off a component.
    pub(crate) fn of(m: &floptle_core::Matter) -> Option<Self> {
        match m {
            floptle_core::Matter::Sprite { ppu, size, cell, flip_x, flip_y, pivot } => Some(Self {
                ppu: *ppu,
                size: *size,
                cell: *cell,
                flip_x: *flip_x,
                flip_y: *flip_y,
                pivot: *pivot,
            }),
            _ => None,
        }
    }

    /// …and the component they describe.
    pub(crate) fn matter(&self) -> floptle_core::Matter {
        floptle_core::Matter::Sprite {
            ppu: self.ppu,
            size: self.size,
            cell: self.cell,
            flip_x: self.flip_x,
            flip_y: self.flip_y,
            pivot: self.pivot,
        }
    }

    /// Fold one `setSprite`-shaped write in, clamping the way the component does.
    ///
    /// The ONE place both the clamps and the keep-what-you-had rule live: the
    /// ECS write and the mirror a script reads straight back both go through
    /// here, so what a script sets and what the renderer draws cannot drift.
    /// Anything that is not a sprite write is ignored rather than refused —
    /// callers hand this whatever they queued.
    pub(crate) fn apply(&mut self, set: &RichSet) {
        let RichSet::MatterSprite { ppu, size, cell, flip_x, flip_y, pivot_x, pivot_y } = set
        else {
            return;
        };
        // `ppu = 0` is meaningful — "size me by `size` instead" — so the floor
        // is zero, not one pixel. `size` cannot be zero: a quad with no edge is
        // nothing on screen and the scale divides by it.
        if let Some(v) = *ppu {
            self.ppu = v.max(0.0);
        }
        if let Some(v) = *size {
            self.size = v.max(1e-4);
        }
        if let Some(v) = *cell {
            self.cell = v;
        }
        if let Some(v) = *flip_x {
            self.flip_x = v;
        }
        if let Some(v) = *flip_y {
            self.flip_y = v;
        }
        // One axis at a time, and the other keeps what it had: `pivotY = 0` is
        // the documented way to stand a character on its feet, and defaulting
        // the axis it did not name would silently recentre it.
        if let Some(v) = *pivot_x {
            self.pivot[0] = v;
        }
        if let Some(v) = *pivot_y {
            self.pivot[1] = v;
        }
    }
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
    /// See [`ShaderTextureSets`]. Separate from the uniform queue because a
    /// texture write is a REBIND, not a buffer write — the two cost different
    /// things and the driver treats them differently.
    shader_texture_sets: ShaderTextureSets,
    screen_shader_toggles: ScreenShaderToggles,
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
    /// Script kinds that FAILED TO LOAD this session. A broken script and a
    /// script with no such export both read `nil` through a handle, and the two
    /// want completely different fixes — so a read against a name in here says
    /// which one it is, once per `(script, key)` (`floptle/0086`).
    broken: Rc<RefCell<std::collections::HashSet<String>>>,
    /// `(script kind, key)` combos already told they were reading from a broken
    /// script, so a handle polled every frame is one Console line.
    broken_read_warned: Rc<RefCell<std::collections::HashSet<(String, String)>>>,
    /// Names a `find*` came up empty on while a SWITCHED-OFF node of that name
    /// existed — said once each, because a lookup in `update` would otherwise
    /// say it every frame.
    ///
    /// This exists because enabled-only is a change of behaviour, and a change
    /// of behaviour that shows up as `nil` is the worst kind: you go and look
    /// for the bug in your own script. One line naming the node it skipped and
    /// the option that brings it back turns it into a five-second fix.
    find_scope_warned: Rc<RefCell<std::collections::HashSet<String>>>,
    /// Lookups that came back empty and have already said so — keyed by the
    /// call plus what it was asked for, so a `getscript` polled in `update`
    /// costs one Console line rather than sixty a second.
    ///
    /// The engine's most expensive bug shape is a reference call that answers
    /// `nil` and says nothing: the symptom lands in somebody else's script,
    /// several frames later, as a value that was never set. Every miss that can
    /// name a likely cause routes through here.
    miss_warned: Rc<RefCell<std::collections::HashSet<String>>>,
    /// The Console feed, shared with the host — a handle read is the one place
    /// in the reference layer that has something to say.
    logs: Rc<RefCell<Vec<ScriptLog>>>,
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
    /// (`docs/multiplayer.md` §3).
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

    /// The read list and the write list of a Material are one list
    /// (`floptle/0082`'s rule, applied here): a field the mirror publishes and
    /// the applier ignores is a value a script can read, assign, and watch do
    /// nothing.
    ///
    /// It matters more since a per-object override is CREATED by a write: the
    /// write list is what decides whether a name is real enough to bring one
    /// into being, so a name in one list and not the other either blanks a part
    /// of a model on a typo or refuses a field that works everywhere else.
    #[test]
    fn every_material_field_can_be_both_read_and_written() {
        let m = floptle_core::Material::default();
        let readable = crate::api::material_fields_for_test(&m, 0);
        let writable: std::collections::HashSet<&str> =
            crate::api::MATERIAL_NUM_FIELDS.iter().copied().collect();
        let missing: Vec<&String> =
            readable.keys().filter(|k| !writable.contains(k.as_str())).collect();
        assert!(
            missing.is_empty(),
            "the mirror publishes {missing:?}, which no write can reach — a script can read \
             them, assign them and watch nothing happen"
        );
        // The other direction, minus the write-only spellings that are
        // deliberately aliases of a published field.
        let aliases = ["opacity"];
        let unreadable: Vec<&&str> = crate::api::MATERIAL_NUM_FIELDS
            .iter()
            .filter(|k| !readable.contains_key(**k) && !aliases.contains(k))
            .collect();
        assert!(unreadable.is_empty(), "writable but never readable: {unreadable:?}");
    }

    /// A TINT is a multiplier over whatever a node already draws — the "same
    /// model, but red" a Material cannot express, because a Material replaces.
    ///
    /// Asked for as: *"there still needs to be an easy way to apply a tint to an
    /// entire mesh without having to manually set everything."* One call, no
    /// Material required, and the model keeps its own textures.
    #[test]
    fn a_script_tints_a_whole_model_without_replacing_anything() {
        use floptle_core::{Material, Matter, Tint};

        let dir = std::env::temp_dir().join(format!("floptle-tint-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_script(
            &dir,
            "flash",
            concat!(
                "function start(node)\n",
                "  node:setTint(color(1, 0.3, 0.3))\n",
                "end\n",
                "function update(node, dt)\n",
                // …and the same value through the component route, which is
                // what an animation lane keys.
                "  local t = node:getcomponent('Tint')\n",
                "  log('alpha=' .. tostring(t and t.alpha or 'none'))\n",
                "  if clear then node:setTint() end\n",
                "end\n",
            ),
        );

        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Matter::Mesh { asset_path: "models/avatar.glb".into() });
        // A material with a texture, to prove the tint leaves it alone.
        world.insert(
            e,
            Material { texture: Some("art/skin.png".into()), ..Material::default() },
        );
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "flash".into(),
                enabled: true,
                params: vec![],
                refs: vec![],
                strs: Vec::new(),
            }]),
        );

        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let t = world.get::<Tint>(e).copied().expect("the tint landed");
        assert!((t.color[0] - 1.0).abs() < 1e-5 && (t.color[1] - 0.3).abs() < 1e-5);
        assert_eq!(t.alpha, 1.0, "no alpha given means opaque, not invisible");
        // The material it was wearing is untouched — that is the whole point.
        let m = world.get::<Material>(e).expect("still has its material");
        assert_eq!(m.texture.as_deref(), Some("art/skin.png"));
        assert_eq!(m.color, [1.0, 1.0, 1.0], "a tint is not a material write");

        // The component route sees it…
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        let logs: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
        assert!(logs.iter().any(|l| l == "alpha=1.0" || l == "alpha=1"), "{logs:?}");

        // …and clearing puts the node back to carrying no tint at all, rather
        // than to carrying a white one nobody asked for.
        let e2 = world.spawn();
        world.insert(e2, Transform::IDENTITY);
        world.insert(e2, Tint { color: [1.0, 0.0, 0.0], alpha: 0.5 });
        world.insert(
            e2,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "clearer".into(),
                enabled: true,
                params: vec![],
                refs: vec![],
                strs: Vec::new(),
            }]),
        );
        write_script(&dir, "clearer", "function start(node)\n  node:setTint()\nend\n");
        host.run(&mut world, &dir, 1.0 / 60.0, 2.0 / 60.0);
        assert!(world.get::<Tint>(e2).is_none(), "setTint() with nothing clears it");
    }

    /// **A clothing system, in script.** `node:materials()` says what the parts
    /// are called; `node:material(name)` is one of them, read and assigned.
    ///
    /// The ask, verbatim: *"for my clothing system I could swap the texture for
    /// the arms and torso for the shirt and swap the texture for the legs for
    /// the pants, and I could do that with a script."* None of it was reachable:
    /// a script could set the NODE's material (which covers the whole model) and
    /// there was no way to name one part, no way to find out what the parts were
    /// called, and `mat.texture` read back nil however many times it had been
    /// written.
    #[test]
    fn a_script_dresses_one_part_of_a_model() {
        use floptle_core::{Material, Matter, ObjectMaterials};

        let dir = std::env::temp_dir().join(format!("floptle-clothing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_script(
            &dir,
            "wardrobe",
            concat!(
                "function start(node)\n",
                // Discovery first — the part names are the model's, not the
                // script author's guess.
                "  for _, slot in ipairs(node:materials()) do\n",
                "    log(slot.material .. ' on ' .. slot.object .. ' textured=' .. \n",
                "        tostring(slot.textured) .. ' overridden=' .. tostring(slot.overridden))\n",
                "  end\n",
                // The shirt goes on every part wearing the Clothing material…
                "  local shirt = node:material('Clothing')\n",
                // Reading BEFORE writing: a part with no override yet reads as
                // the default material, so the ordinary first line anybody
                // writes — halve what is there — is arithmetic and not a raise.
                "  log('fresh alpha=' .. tostring(shirt.alpha))\n",
                "  shirt.alpha = shirt.alpha * 0.5\n",
                "  shirt.texture = 'art/shirt.png'\n",
                "  shirt.color = color(1, 0.9, 0.9)\n",
                // …the trousers on one named object…
                "  node:material('RightLeg#2').texture = 'art/pants.png'\n",
                // …and the whole-model Material stays what it was.
                "  node:material().roughness = 0.25\n",
                // Read-your-writes, on a string, which used to answer nil.
                "  log('wearing ' .. tostring(shirt.texture))\n",
                "end\n",
            ),
        );

        let mut world = World::default();
        let hero = world.spawn();
        world.insert(hero, Transform::IDENTITY);
        world.insert(hero, Matter::Mesh { asset_path: "models/avatar.glb".into() });
        world.insert(hero, Material::default());
        world.insert(
            hero,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "wardrobe".into(),
                enabled: true,
                params: vec![],
                refs: vec![],
                strs: Vec::new(),
            }]),
        );

        let mut host = ScriptHost::new();
        // What the editor lends: the parts this model was imported with.
        host.set_model_slots(std::collections::HashMap::from([(
            "models/avatar.glb".to_string(),
            vec![
                crate::ModelSlot {
                    object: "Torso#2".into(),
                    material: "Clothing".into(),
                    textured: true,
                },
                crate::ModelSlot {
                    object: "RightLeg#2".into(),
                    material: "Pants".into(),
                    textured: true,
                },
            ],
        )]));
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

        let logs: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
        assert!(
            logs.iter().any(|l| l == "Clothing on Torso#2 textured=true overridden=false"),
            "a script must be able to ASK what the parts are called: {logs:?}"
        );
        assert!(
            logs.iter().any(|l| l == "fresh alpha=1.0" || l == "fresh alpha=1"),
            "a part with no override yet reads as the material it is about to become: {logs:?}"
        );
        assert!(
            logs.iter().any(|l| l == "wearing art/shirt.png"),
            "a material's texture has to read back — a field you can only write is half a \
             field: {logs:?}"
        );

        // …and the writes landed on the component the renderer reads.
        let om = world.get::<ObjectMaterials>(hero).expect("the overrides were created");
        let shirt = om.0.get("Clothing").expect("the material-name slot");
        assert_eq!(shirt.texture.as_deref(), Some("art/shirt.png"));
        assert!((shirt.color[1] - 0.9).abs() < 1e-5, "the colour went on as a colour: {:?}", shirt.color);
        assert!((shirt.alpha - 0.5).abs() < 1e-5, "read-then-write landed: {}", shirt.alpha);
        assert_eq!(
            om.0.get("RightLeg#2").and_then(|m| m.texture.as_deref()),
            Some("art/pants.png"),
            "an object name addresses one part"
        );
        // The node's own Material is still the node's own.
        let node_mat = world.get::<Material>(hero).expect("still there");
        assert!((node_mat.roughness - 0.25).abs() < 1e-5);
        assert_eq!(node_mat.texture, None, "dressing a part must not touch the whole model");
    }

    /// A LIBRARY script — no `start`, no `update`, just functions other scripts
    /// call — must have its `params` before anybody calls into it
    /// (`floptle/0156`).
    ///
    /// `params` used to be seeded by the tick, so a script that never ticks
    /// never got one and the first caller into any of its functions got
    /// `attempt to index global 'params' (a nil value)`. Worse than a plain nil:
    /// whether a hookless script had been seeded depended on whether something
    /// else had ticked it first, which depends on SCENE ORDER — so the same
    /// project worked on one machine and raised on another, and adding an
    /// unrelated node could fix it. In the solar game this one error read as
    /// four broken features (no inventory, no selling, no HUD count).
    #[test]
    fn a_hookless_library_script_has_its_params_before_anybody_calls_in() {
        let dir = std::env::temp_dir().join(format!("floptle-libparams-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // No start, no update: this script exists to be CALLED.
        write_script(
            &dir,
            "inventory",
            concat!(
                // `@node` is a REFERENCE param: the Inspector wires it to a node
                // and the script reads it as a handle.
                "defaults = { cap = 40, owner = noderef() }\n",
                "function cap()\n",
                "  return params.cap\n",
                "end\n",
                "function ownerName()\n",
                "  return params.owner and params.owner.name or 'nobody'\n",
                "end\n",
            ),
        );
        // …and the caller asks on the very first frame, from `start`.
        write_script(
            &dir,
            "hud",
            concat!(
                "function start(node)\n",
                "  local inv = findScript('inventory')\n",
                "  log('cap=' .. tostring(inv:cap()))\n",
                "  log('owner=' .. tostring(inv:ownerName()))\n",
                "end\n",
            ),
        );

        let mut world = World::default();
        // The library node comes SECOND in scene order on purpose: it is the
        // order that used to decide whether this worked.
        let hud = world.spawn();
        world.insert(hud, Transform::IDENTITY);
        world.insert(hud, floptle_core::Name("Hud".into()));
        world.insert(
            hud,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "hud".into(),
                enabled: true,
                params: vec![],
                refs: vec![],
                strs: Vec::new(),
            }]),
        );
        let bag = world.spawn();
        world.insert(bag, Transform::IDENTITY);
        world.insert(bag, floptle_core::Name("Inventory".into()));
        world.insert(
            bag,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "inventory".into(),
                enabled: true,
                // The Inspector's own value, which is what the caller must read
                // — not the `defaults` line and not nil.
                params: vec![("cap".into(), 55.0)],
                // …wired to the HUD node, by name, as the Inspector wires one.
                refs: vec![("owner".into(), "Hud".into())],
                strs: Vec::new(),
            }]),
        );

        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let logs: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
        assert!(
            logs.iter().any(|l| l == "cap=55.0" || l == "cap=55"),
            "the library answered from its Inspector params: {logs:?}"
        );
        // A hookless script NEVER ticks, so the seed is the only chance its
        // reference params ever get to be resolved.
        assert!(
            logs.iter().any(|l| l == "owner=Hud"),
            "a wired reference param has to be there too, or it is nil forever: {logs:?}"
        );
    }

    /// `terrain.busy()` (floptle/0158) has to be true the MOMENT work is
    /// queued, not on the frame after.
    ///
    /// The consumer is a game that builds its world as the player travels: it
    /// queues one system, then asks whether it may queue the next. Answering
    /// with last frame's state means the answer to "did what I just asked for
    /// start?" is "no" — so the game queues it again, and the second request is
    /// the one that lands behind the ground somebody is standing on. The flag is
    /// therefore raised by the QUEUEING call itself; the editor's per-frame
    /// publish then owns it from the real job state and is what lowers it again.
    #[test]
    fn terrain_busy_is_true_the_moment_a_fill_is_queued() {
        let dir = std::env::temp_dir().join(format!("floptle-busy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_script(
            &dir,
            "galaxy",
            concat!(
                "function update(node, dt)\n",
                "  if not asked then\n",
                "    log('before=' .. tostring(terrain.busy()))\n",
                "    terrain.generatePlanet(1, { radius = 20 })\n",
                "    log('after=' .. tostring(terrain.busy()))\n",
                "    asked = true\n",
                "  else\n",
                "    log('later=' .. tostring(terrain.busy()))\n",
                "  end\n",
                "end\n",
            ),
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "galaxy".into(),
                enabled: true,
                params: vec![],
                refs: vec![],
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let logs: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
        assert!(logs.iter().any(|l| l == "before=false"), "idle to start with: {logs:?}");
        assert!(
            logs.iter().any(|l| l == "after=true"),
            "the queueing call itself must raise it — a game that queues and then asks in the \
             same breath is the whole consumer: {logs:?}"
        );

        // What the editor does with the queue, and then its per-frame publish
        // finding nothing left to do. The flag is the WORKER's state, so the
        // host is what lowers it — and the script sees that on the next tick.
        let queued = host.take_terrain_generates();
        assert_eq!(queued.len(), 1, "the fill was queued for the editor to drain");
        host.set_terrain_busy(false);
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        let logs: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
        assert!(logs.iter().any(|l| l == "later=false"), "quiet again once nothing is running: {logs:?}");
    }

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
        host.call_create_callback(&mut world, cb, child);
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
        // The gameplay-tick hook (docs/multiplayer.md §3): `fixedUpdate(node, dt)`
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
        // The Lua net.* bridge (docs/multiplayer.md §8): rpc queueing with
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

    /// The two-material floor a `floptle/0174` test needs: `x < 0` is "Grass",
    /// `x > 0` is "Boards", tagged as node 7.
    fn labelled_floor(eid: u32) -> floptle_physics::AnchoredCollider {
        let verts = [
            glam::Vec3::new(-4.0, 0.0, -4.0),
            glam::Vec3::new(0.0, 0.0, -4.0),
            glam::Vec3::new(0.0, 0.0, 4.0),
            glam::Vec3::new(-4.0, 0.0, 4.0),
            glam::Vec3::new(4.0, 0.0, -4.0),
            glam::Vec3::new(4.0, 0.0, 4.0),
        ];
        let mut c = floptle_physics::AnchoredCollider::world(Box::new(
            floptle_physics::TriMeshCollider::labelled(
                &verts,
                &[0, 1, 2, 0, 2, 3, 1, 4, 5, 1, 5, 2],
                &[0, 0, 1, 1],
                vec!["Grass".into(), "Boards".into()],
            ),
        ));
        c.eid = Some(eid);
        c
    }

    /// **The whole of `floptle/0174`, from a script.**
    ///
    /// A first-person game asks "what am I standing on" to pick a footstep. It
    /// got two wrong answers. `raycast` returned no `hit.node` at all for static
    /// geometry — so the level, which is all static geometry, was invisible to
    /// the one query the docs point people at. And the best available answer,
    /// the node's own material, is one material per node: a mansion that is one
    /// map mesh with nine slots reported stone for its grass, its boards and its
    /// wallpaper alike.
    ///
    /// Both are read here the way a footstep script reads them.
    #[test]
    fn a_script_can_ask_what_surface_it_is_standing_on() {
        let dir = std::env::temp_dir().join("floptle_script_test_surface");
        let _ = std::fs::create_dir_all(&dir);
        // Cast down from above each half of the floor and record what came back.
        write_script(
            &dir,
            "footsteps",
            "function update(node, dt)\n            \x20 local l = raycast(-2, 5, 0, 0, -1, 0, 20)\n            \x20 local r = raycast(2, 5, 0, 0, -1, 0, 20)\n            \x20 local s = spherecast(vec3(2, 5, 0), vec3(0, -1, 0), 0.2, 20)\n            \x20 node.strs = {\n            \x20   left = l and l.material or \"nil\",\n            \x20   right = r and r.material or \"nil\",\n            \x20   sphere = s and s.material or \"nil\",\n            \x20   node = (l and l.node) and \"yes\" or \"no\",\n            \x20 }\n            \x20 print(node.strs.left .. \"|\" .. node.strs.right .. \"|\" .. node.strs.sphere .. \"|\" .. node.strs.node)\n            end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "footsteps".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.set_colliders(vec![labelled_floor(7)], glam::DVec3::ZERO);
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let _ = host.take_colliders();
        let said =
            host.drain_logs().into_iter().map(|l| l.msg).collect::<Vec<_>>().join("\n");
        assert!(
            said.contains("Grass|Boards|Boards|yes"),
            "a script asked what it was standing on and got {said:?} — it should read the \
             floor's own material on each side, and name the node it hit"
        );
    }

    /// **A script with no hook for this pass is not charged for the frame.**
    ///
    /// Per-script timing is a wall clock around the call, so whatever the
    /// machine does inside that span — a garbage collection, the OS taking the
    /// core away — is reported as that script's cost. For a script with no
    /// `update` at all the span wraps nothing, and a game profiling itself
    /// found 12–21 ms "peaks" against a file that could not have spent them.
    /// Anybody reading that goes and optimises the wrong file, which is worse
    /// than having no per-script numbers at all.
    #[test]
    fn a_script_with_no_update_is_not_charged_for_the_frame() {
        let dir = std::env::temp_dir().join("floptle_script_test_perf_attrib");
        let _ = std::fs::create_dir_all(&dir);
        // One script that does real work, and one with no hook for this pass at
        // all — the shape that was being blamed.
        write_script(
            &dir,
            "worker",
            "function update(node, dt)\n  local s = 0\n  for i = 1, 20000 do s = s + i end\n               node.y = s * 0.0\nend\n",
        );
        write_script(&dir, "marker", "-- a data-only script: no hooks at all\nfoo = 1\n");

        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![
                floptle_core::ScriptInst {
                    kind: "worker".into(),
                    enabled: true,
                    params: vec![],
                    refs: Vec::new(),
                    strs: Vec::new(),
                },
                floptle_core::ScriptInst {
                    kind: "marker".into(),
                    enabled: true,
                    params: vec![],
                    refs: Vec::new(),
                    strs: Vec::new(),
                },
            ]),
        );
        let mut host = ScriptHost::new();
        host.profile().borrow_mut().enable(true);
        for _ in 0..3 {
            host.run(&mut world, &dir, 0.016, 0.016);
            host.profile().borrow_mut().end_frame();
        }
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

        let by_script = host.profile().borrow().scripts();
        let named: Vec<&str> = by_script.iter().map(|(n, _)| n.as_str()).collect();
        assert!(named.contains(&"worker"), "the script that did the work is missing: {named:?}");
        assert!(
            !named.contains(&"marker"),
            "a script with no hook was charged for the frame it was not in: {named:?}"
        );
    }

    /// **What the driver feeds is what a script reads.**
    ///
    /// The other `app` tests here all set a value and read it back, which passes
    /// even if the initial state never arrives — a menu would open showing
    /// defaults rather than the game's real settings, and only the controls
    /// somebody touched would ever be right.
    #[test]
    fn a_menu_opens_showing_the_settings_the_game_actually_has() {
        let dir = std::env::temp_dir().join("floptle_script_test_app_initial");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "menu",
            "function update(node, dt)\n\
            \x20 print(app.title() .. \"|\" .. app.version() .. \"|\" .. app.vsync()\n\
            \x20   .. \"|\" .. tostring(app.retro()) .. \"|\" .. app.retroHeight())\n\
            end\n",
        );
        let (mut world, _e) = world_with_script("menu");
        let mut host = ScriptHost::new();
        host.set_app_info(crate::app_api::AppInfo {
            title: "Test Game".into(),
            version: "9.9.9".into(),
            vsync: crate::app_api::Vsync::Adaptive,
            retro: true,
            retro_height: 240,
            retro_integer_scale: false,
            fullscreen: false,
        });
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let said = host.drain_logs().into_iter().map(|l| l.msg).collect::<Vec<_>>().join("\n");
        assert!(
            said.contains("Test Game|9.9.9|Adaptive|true|240"),
            "a menu asked what the game is and got {said:?}"
        );
        // Reading changes nothing — a Video tab paints itself every frame.
        assert!(host.take_app_requests().is_empty());
    }

    /// The other half of the same promise: something with no per-face material
    /// answers `nil`, not a plausible wrong name. A name that is right for map
    /// meshes and quietly wrong for terrain is worse than no name at all.
    #[test]
    fn a_surface_with_no_per_face_material_answers_nothing() {
        let dir = std::env::temp_dir().join("floptle_script_test_surface_none");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "asker",
            "function update(node, dt)\n            \x20 local h = raycast(0, 5, 0, 0, -1, 0, 20)\n            \x20 print(h and (h.material or \"nil\") or \"miss\")\n            end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "asker".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        let mut plane =
            floptle_physics::AnchoredCollider::world(Box::new(floptle_physics::Plane::ground(0.0)));
        plane.eid = Some(3);
        host.set_colliders(vec![plane], glam::DVec3::ZERO);
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let _ = host.take_colliders();
        let said =
            host.drain_logs().into_iter().map(|l| l.msg).collect::<Vec<_>>().join("\n");
        assert!(said.contains("nil"), "an analytic plane invented a material: {said:?}");
    }

    /// A collider that counts how many times anything asked it for a surface
    /// label. The whole of criterion 5 in `floptle/0174` is that this stays at
    /// zero for a query nobody asks.
    struct CountsLabelAsks {
        inner: floptle_physics::TriMeshCollider,
        asks: std::sync::atomic::AtomicU32,
    }

    impl floptle_physics::CollisionShape for CountsLabelAsks {
        fn distance(&self, p: glam::Vec3) -> f32 {
            self.inner.distance(p)
        }
        fn normal(&self, p: glam::Vec3) -> glam::Vec3 {
            self.inner.normal(p)
        }
        fn face_label(&self, p: glam::Vec3) -> Option<&str> {
            self.asks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.face_label(p)
        }
    }

    /// **A line-of-sight ray must not pay for a footstep's question.**
    ///
    /// The per-face lookup costs a closest-point search of its own, and the
    /// ordinary ray — cast far more often than a ground check and never
    /// interested in the answer — must not run it. Which is why `hit.material`
    /// is resolved lazily rather than filled in when the hit is built: a script
    /// that never reads the field never triggers the search.
    ///
    /// Counted rather than timed. A timing ratio cannot see one extra
    /// closest-point search against a march of dozens of steps, so it would pass
    /// while the promise was broken.
    #[test]
    fn a_query_that_never_asks_what_it_hit_pays_nothing_for_the_answer() {
        use std::sync::atomic::Ordering;
        let dir = std::env::temp_dir().join("floptle_script_test_surface_cost");
        let _ = std::fs::create_dir_all(&dir);
        // Three queries, and not one of them reads `material`.
        write_script(
            &dir,
            "looker",
            "function update(node, dt)\n\
            \x20 local a = raycast(-2, 5, 0, 0, -1, 0, 20)\n\
            \x20 local b = spherecast(vec3(2, 5, 0), vec3(0, -1, 0), 0.2, 20)\n\
            \x20 local c = overlapSphere(vec3(0, 0, 0), 2)\n\
            \x20 node.y = (a and a.distance or 0) + (b and b.nx or 0) + #c\n\
            end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "looker".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let verts = [
            glam::Vec3::new(-4.0, 0.0, -4.0),
            glam::Vec3::new(0.0, 0.0, -4.0),
            glam::Vec3::new(0.0, 0.0, 4.0),
            glam::Vec3::new(-4.0, 0.0, 4.0),
            glam::Vec3::new(4.0, 0.0, -4.0),
            glam::Vec3::new(4.0, 0.0, 4.0),
        ];
        let shape = std::sync::Arc::new(CountsLabelAsks {
            inner: floptle_physics::TriMeshCollider::labelled(
                &verts,
                &[0, 1, 2, 0, 2, 3, 1, 4, 5, 1, 5, 2],
                &[0, 0, 1, 1],
                vec!["Grass".into(), "Boards".into()],
            ),
            asks: std::sync::atomic::AtomicU32::new(0),
        });
        struct Shared(std::sync::Arc<CountsLabelAsks>);
        impl floptle_physics::CollisionShape for Shared {
            fn distance(&self, p: glam::Vec3) -> f32 {
                self.0.distance(p)
            }
            fn normal(&self, p: glam::Vec3) -> glam::Vec3 {
                self.0.normal(p)
            }
            fn face_label(&self, p: glam::Vec3) -> Option<&str> {
                self.0.face_label(p)
            }
        }
        let mut c = floptle_physics::AnchoredCollider::world(Box::new(Shared(shape.clone())));
        c.eid = Some(7);

        let mut host = ScriptHost::new();
        host.set_colliders(vec![c], glam::DVec3::ZERO);
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let _ = host.take_colliders();
        assert_eq!(
            shape.asks.load(Ordering::Relaxed),
            0,
            "three queries ran and none of them read hit.material, yet the per-face lookup was \
             paid anyway — that cost belongs to the caller that wants the answer"
        );

        // …and the counter is not stuck at zero: asking DOES reach the shape.
        assert_eq!(
            floptle_physics::CollisionShape::face_label(&*shape, glam::Vec3::new(2.0, 0.1, 0.0)),
            Some("Boards")
        );
        assert_eq!(shape.asks.load(Ordering::Relaxed), 1);
    }

    /// **A settings menu reads back what it just set.**
    ///
    /// The driver applies these a moment later — a swap chain and a GPU target
    /// are not things a Lua call can touch — so if `app.vsync()` answered the
    /// old value until then, every control in a Video tab would snap back to its
    /// previous position for a frame after being clicked. That reads as a
    /// control that did not work, which is the whole failure `floptle/0175` is
    /// about (`floptle/0082`'s lesson, one layer up).
    #[test]
    fn a_settings_menu_reads_back_what_it_just_set() {
        let dir = std::env::temp_dir().join("floptle_script_test_app_settings");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "menu",
            "function update(node, dt)\n\
            \x20 app.setVsync(\"Off\")\n\
            \x20 app.setRetroHeight(360)\n\
            \x20 app.setRetroIntegerScale(true)\n\
            \x20 print(app.vsync() .. \"|\" .. app.retroHeight() .. \"|\" .. tostring(app.retroIntegerScale()))\n\
            end\n",
        );
        let (mut world, _e) = world_with_script("menu");
        let mut host = ScriptHost::new();
        host.set_app_info(crate::app_api::AppInfo {
            title: "Game".into(),
            version: "0.0.0".into(),
            vsync: crate::app_api::Vsync::On,
            retro: true,
            retro_height: 240,
            retro_integer_scale: false,
            fullscreen: false,
        });
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let said = host.drain_logs().into_iter().map(|l| l.msg).collect::<Vec<_>>().join("\n");
        assert!(
            said.contains("Off|360|true"),
            "a menu set three settings and read back {said:?} — it must see its own change"
        );
        // …and the driver is told to actually go and do it.
        let req = host.take_app_requests();
        assert_eq!(req.vsync, Some(crate::app_api::Vsync::Off));
        assert_eq!(req.retro_height, Some(360));
        assert_eq!(req.retro_integer_scale, Some(true));
        assert!(!req.quit, "nobody asked to quit");
        // Drained: a request left in the queue would be applied again every
        // frame, which for `quit` is the difference between closing once and
        // never being able to do anything else.
        assert!(host.take_app_requests().is_empty());
    }

    /// `app.quit()` reaches the driver as a request rather than doing anything
    /// itself — there is no event loop to reach from inside a Lua call, and what
    /// quitting MEANS differs between a build, the editor and a headless run.
    #[test]
    fn quit_is_a_request_the_driver_answers() {
        let dir = std::env::temp_dir().join("floptle_script_test_app_quit");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "menu", "function update(node, dt)\n  app.quit()\nend\n");
        let (mut world, _e) = world_with_script("menu");
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert!(host.take_app_requests().quit, "the driver was never told");
        assert!(!host.take_app_requests().quit, "and it must not be told twice");
    }

    /// A mode nobody recognises is named, not ignored. A settings menu that
    /// silently kept the old value would be a control that appears to work.
    #[test]
    fn an_unknown_vsync_mode_is_refused_by_name() {
        let dir = std::env::temp_dir().join("floptle_script_test_app_badmode");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "menu",
            "function update(node, dt)\n\
            \x20 local ok, err = pcall(function() app.setVsync(\"vsync\") end)\n\
            \x20 print(tostring(ok) .. \"|\" .. tostring(err))\n\
            end\n",
        );
        let (mut world, _e) = world_with_script("menu");
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        let said = host.drain_logs().into_iter().map(|l| l.msg).collect::<Vec<_>>().join("\n");
        assert!(said.contains("false|"), "it was accepted: {said:?}");
        assert!(said.contains("\"On\""), "the refusal has to list the modes: {said:?}");
        assert!(host.take_app_requests().vsync.is_none(), "a refused mode must not be queued");
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

    /// A script filed in a subfolder must answer to the name on its tab.
    ///
    /// This is the bug as reported: `scripts/forgery/playermovement.lua` is
    /// stored on the node as the kind `forgery/playermovement`, because a kind
    /// is a path under `scripts/` without the extension. Nothing a person looks
    /// at says so — the editor tab says `playermovement.lua`, the Inspector row
    /// says `playermovement`, the Console prefixes its output `playermovement:22`
    /// — so `node:getscript("playermovement")` is what everybody writes, and it
    /// matched nothing and returned `nil` with no complaint. The `nil` then
    /// surfaced two scripts away as a value that was never set.
    #[test]
    fn a_script_in_a_subfolder_answers_to_its_bare_name() {
        let dir = std::env::temp_dir().join("floptle_script_test_kind_stem");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("forgery")).unwrap();
        write_script(&dir, "forgery/playermovement", "state = \"idle\"\n");
        write_script(
            &dir,
            "reader",
            "function update(node, dt)\n  \
             local m = findTagged(\"Player\")[1]:getscript(\"playermovement\")\n  \
             if m and m.state == \"idle\" then node.x = 1 end\n\
             end\n",
        );

        let mut world = World::default();
        let player = world.spawn();
        world.insert(player, Transform::IDENTITY);
        world.insert(player, floptle_core::Tags(vec!["Player".into()]));
        world.insert(
            player,
            Scripts(vec![floptle_core::ScriptInst::new("forgery/playermovement")]),
        );
        let reader = world.spawn();
        world.insert(reader, Transform::IDENTITY);
        world.insert(reader, Scripts(vec![floptle_core::ScriptInst::new("reader")]));

        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(
            world.get::<Transform>(reader).unwrap().translation.x,
            1.0,
            "the bare file name must reach a script filed in a folder"
        );
    }

    /// …and the full path still means exactly what it meant, so a project that
    /// already spells them out is untouched.
    #[test]
    fn the_full_script_path_still_resolves() {
        let dir = std::env::temp_dir().join("floptle_script_test_kind_path");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("forgery")).unwrap();
        write_script(&dir, "forgery/mover", "state = 7\n");
        write_script(
            &dir,
            "reader",
            "function update(node, dt)\n  \
             local m = findScript(\"forgery/mover\")\n  \
             if m then node.x = m.state end\n\
             end\n",
        );
        let mut world = World::default();
        let a = world.spawn();
        world.insert(a, Transform::IDENTITY);
        world.insert(a, Scripts(vec![floptle_core::ScriptInst::new("forgery/mover")]));
        let b = world.spawn();
        world.insert(b, Transform::IDENTITY);
        world.insert(b, Scripts(vec![floptle_core::ScriptInst::new("reader")]));
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(world.get::<Transform>(b).unwrap().translation.x, 7.0);
    }

    /// Two files with the same name in different folders: refuse, naming both.
    /// Picking one would be a coin flip whose loser is silent.
    #[test]
    fn an_ambiguous_bare_script_name_is_refused_by_name() {
        let dir = std::env::temp_dir().join("floptle_script_test_kind_ambig");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        write_script(&dir, "a/thing", "who = \"a\"\n");
        write_script(&dir, "b/thing", "who = \"b\"\n");
        write_script(
            &dir,
            "reader",
            "function update(node, dt)\n  local _ = findScript(\"thing\")\nend\n",
        );
        let mut world = World::default();
        for k in ["a/thing", "b/thing", "reader"] {
            let e = world.spawn();
            world.insert(e, Transform::IDENTITY);
            world.insert(e, Scripts(vec![floptle_core::ScriptInst::new(k)]));
        }
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        let errs = host.errors().join("\n");
        assert!(errs.contains("a/thing") && errs.contains("b/thing"), "{errs}");
    }

    /// A `getscript` that finds nothing says what the node DOES carry — once,
    /// however many frames poll it.
    #[test]
    fn a_getscript_miss_names_what_the_node_carries() {
        let dir = std::env::temp_dir().join("floptle_script_test_getscript_miss");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "held", "hp = 1\n");
        write_script(
            &dir,
            "reader",
            "function update(node, dt)\n  local _ = node:getscript(\"helth\")\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, floptle_core::Name("Hero".into()));
        world.insert(
            e,
            Scripts(vec![
                floptle_core::ScriptInst::new("held"),
                floptle_core::ScriptInst::new("reader"),
            ]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        let said: Vec<String> = host
            .drain_logs()
            .into_iter()
            .filter(|l| l.level == LogLevel::Warn)
            .map(|l| l.msg)
            .collect();
        assert_eq!(said.len(), 1, "exactly one line: {said:?}");
        assert!(said[0].contains("Hero") && said[0].contains("held"), "{}", said[0]);
        // Three more frames of the same miss must stay at one line.
        host.run(&mut world, &dir, 0.1, 0.2);
        host.run(&mut world, &dir, 0.1, 0.3);
        assert!(
            host.drain_logs().iter().all(|l| l.level != LogLevel::Warn),
            "a miss polled every frame is still one Console line"
        );
    }

    /// Every other node method is camelCase (`hasTag`, `setWorldPos`,
    /// `distanceTo`), so `node:getChild` / `node:getParent` / `node:getScript`
    /// is what gets written — and all three used to die at the call with
    /// "attempt to call method 'getChild' (a nil value)", which names the
    /// symptom and nothing to do about it.
    #[test]
    fn the_get_node_methods_take_the_camel_case_spelling() {
        let dir = std::env::temp_dir().join("floptle_script_test_camel_get");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "kid", "hp = 3\n");
        write_script(
            &dir,
            "reader",
            "function update(node, dt)\n  \
             local k = node:getChild(\"Kid\")\n  \
             if k and k:getParent().name == \"Root\" then node.x = k:getScript(\"kid\").hp end\n\
             end\n",
        );
        let mut world = World::default();
        let root = world.spawn();
        world.insert(root, Transform::IDENTITY);
        world.insert(root, floptle_core::Name("Root".into()));
        world.insert(root, Scripts(vec![floptle_core::ScriptInst::new("reader")]));
        let kid = world.spawn();
        world.insert(kid, Transform::IDENTITY);
        world.insert(kid, floptle_core::Name("Kid".into()));
        world.insert(kid, floptle_core::Parent(root));
        world.insert(kid, Scripts(vec![floptle_core::ScriptInst::new("kid")]));
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(world.get::<Transform>(root).unwrap().translation.x, 3.0);
    }

    /// A CASING slip on any other node method names its fix rather than dying
    /// at the call site. Genuinely unknown keys still read nil, so a feature
    /// probe (`if node.someday then`) keeps working.
    #[test]
    fn a_node_method_casing_slip_names_the_fix() {
        let dir = std::env::temp_dir().join("floptle_script_test_node_casing");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "typo",
            "function update(node, dt)\n  \
             if dt < 0.15 then\n    if node.someday == nil then node.y = 4 end\n  \
             else\n    node.x = node:HasTag(\"x\") and 1 or 0\n  end\n\
             end\n",
        );
        let (mut world, e) = world_with_script("typo");
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.errors().is_empty(), "a nil probe must not raise: {:?}", host.errors());
        assert_eq!(world.get::<Transform>(e).unwrap().translation.y, 4.0);
        host.run(&mut world, &dir, 0.2, 0.3);
        let errs = host.errors().join("\n");
        assert!(errs.contains("did you mean `hasTag`"), "{errs}");
    }

    /// `findTagged(...)[0]` is the first hour of every Lua API, and the engine's
    /// answer was a `nil` that died one call later as "attempt to index a nil
    /// value" — pointing at the line, saying nothing about the cause. An index
    /// below 1 is never an element of a Lua list, so it can say so outright.
    /// A positive index past the end still reads nil: `if findTagged("x")[1]`
    /// is how you ask whether there are any.
    #[test]
    fn indexing_a_result_list_from_zero_says_lists_start_at_one() {
        let dir = std::env::temp_dir().join("floptle_script_test_zero_index");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "zero",
            "function update(node, dt)\n  \
             if dt < 0.15 then\n    \
             if findTagged(\"me\")[9] == nil then node.y = 2 end\n  \
             else\n    local _ = findTagged(\"me\")[0]\n  end\n\
             end\n",
        );
        let (mut world, e) = world_with_script("zero");
        world.insert(e, floptle_core::Tags(vec!["me".into()]));
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.errors().is_empty(), "past-the-end must stay nil: {:?}", host.errors());
        assert_eq!(world.get::<Transform>(e).unwrap().translation.y, 2.0);
        host.run(&mut world, &dir, 0.2, 0.3);
        let errs = host.errors().join("\n");
        assert!(errs.contains("1-based") && errs.contains("[0]"), "{errs}");
    }

    /// A script that is attached but switched OFF reads nil through a handle,
    /// exactly like a live script with no such export. They want completely
    /// different fixes, so the handle says which one it is.
    #[test]
    fn reading_a_switched_off_script_says_it_is_switched_off() {
        let dir = std::env::temp_dir().join("floptle_script_test_off_script");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "engine", "power = 9\n");
        write_script(
            &dir,
            "reader",
            "function update(node, dt)\n  local _ = node:getscript(\"engine\").power\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![
                floptle_core::ScriptInst { enabled: false, ..floptle_core::ScriptInst::new("engine") },
                floptle_core::ScriptInst::new("reader"),
            ]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        let said = host
            .drain_logs()
            .into_iter()
            .filter(|l| l.level == LogLevel::Warn)
            .map(|l| l.msg)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(said.contains("attached but not running"), "{said}");
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

    /// **`params` is built when its seed changes, not on every hook call.**
    ///
    /// The table used to be rebuilt from the ECS seed on every pass of every
    /// instance — three times a frame per scripted node — and on a scene of
    /// sixty scripted nodes that was most of what a node cost. It is now
    /// fingerprinted. The guard is a count, watched: force the rebuild back on
    /// and this reads twenty instead of one.
    #[test]
    fn params_are_rebuilt_only_when_the_seed_changes() {
        let dir = std::env::temp_dir().join(format!("floptle_seedfp_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "tunable",
            "defaults = { speed = 1 }\n\
             function update(node, dt) node.y = params.speed end\n\
             function fixedUpdate(node, dt) end\n",
        );
        let (mut world, e) = world_with_script("tunable");
        let mut host = ScriptHost::new();
        crate::host::PARAMS_REBUILDS.with(|c| c.set(0));
        for i in 0..10 {
            let t = i as f32 / 60.0;
            host.run(&mut world, &dir, 1.0 / 60.0, t);
            host.run_fixed(&mut world, 1.0 / 60.0, t);
        }
        assert_eq!(
            crate::host::PARAMS_REBUILDS.with(|c| c.get()),
            1,
            "an unchanged seed must build the table once, not once per hook call"
        );
        assert_eq!(world.get::<Transform>(e).unwrap().translation.y, 1.0, "the default reached the script");

        // The editor changes the seed: the next pass must see it — and cost
        // exactly one more build.
        world.get_mut::<Scripts>(e).unwrap().0[0].params.push(("speed".into(), 2.0));
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0);
        assert_eq!(crate::host::PARAMS_REBUILDS.with(|c| c.get()), 2, "a changed seed rebuilds once");
        assert_eq!(world.get::<Transform>(e).unwrap().translation.y, 2.0, "the new seed reached the script");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A pass the script has no hook for still drains a write made through a
    /// stashed handle.** The hook-less fast path skips the params table and
    /// the write scan; it must NOT skip the node — a timer callback writing
    /// `me.x = 5` through a handle kept from `start()` has to reach the world on
    /// the very next pass, exactly as it did when every pass paid full price.
    #[test]
    fn a_hookless_pass_still_drains_a_timer_write_to_the_node() {
        let dir = std::env::temp_dir().join(format!("floptle_hookless_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "stasher",
            "local me\n\
             function start(node)\n\
               me = node\n\
               after(0.01, function() me.x = 5 end)\n\
             end\n",
        );
        let (mut world, e) = world_with_script("stasher");
        let mut host = ScriptHost::new();
        // Frame pass: `start` runs, stashes the handle, arms the timer.
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 0.0);
        // Tick pass: the timer fires in the scheduler, BEFORE the script pass —
        // and this script has no `fixedUpdate`, so the pass takes the light
        // path. The write must still land.
        host.run_fixed(&mut world, 1.0 / 60.0, 1.0 / 60.0);
        assert_eq!(
            world.get::<Transform>(e).unwrap().translation.x,
            5.0,
            "a hook-less pass dropped a write made through a stashed node handle"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **When only transforms moved, the mirror is refreshed, not rebuilt** —
    /// and the refresh is real: a handle reads the moved value.
    ///
    /// Three full rebuilds a frame were most of what a large scene cost the
    /// script host outside the hooks. The guard is a count of rebuilds, and it
    /// is watched in both directions with the rename test below.
    #[test]
    fn a_transform_only_change_refreshes_the_mirror_without_a_rebuild() {
        let dir = std::env::temp_dir().join(format!("floptle_mirror_refresh_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "reader", "function update(node, dt) node.y = find('Other').x end\n");
        let (mut world, e) = world_with_script("reader");
        let other = world.spawn();
        world.insert(other, Transform::IDENTITY);
        world.insert(other, floptle_core::Name("Other".into()));
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        crate::host::FULL_SYNCS.with(|c| c.set(0));

        // Physics-shaped change: a transform, through `get_mut`, nothing else.
        world.get_mut::<Transform>(other).unwrap().translation.x = 7.0;
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        host.run_fixed(&mut world, 1.0 / 60.0, 1.0 / 60.0);
        host.run_late(&mut world, 1.0 / 60.0, 1.0 / 60.0);
        assert_eq!(
            crate::host::FULL_SYNCS.with(|c| c.get()),
            0,
            "three passes over a transform-only change must not rebuild the mirror"
        );
        assert_eq!(
            world.get::<Transform>(e).unwrap().translation.y,
            7.0,
            "the refresh did not carry the moved transform to a handle"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Anything that is not a transform forces a full rebuild** — here a
    /// rename, the kind of change the refresh path cannot see and must never
    /// be allowed to hide.
    #[test]
    fn a_rename_forces_a_full_rebuild_and_find_sees_it() {
        let dir = std::env::temp_dir().join(format!("floptle_mirror_rename_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "seeker", "function update(node, dt) node.y = find('Renamed') and 1 or 0 end\n");
        let (mut world, e) = world_with_script("seeker");
        let other = world.spawn();
        world.insert(other, Transform::IDENTITY);
        world.insert(other, floptle_core::Name("Other".into()));
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert_eq!(world.get::<Transform>(e).unwrap().translation.y, 0.0, "not renamed yet");
        crate::host::FULL_SYNCS.with(|c| c.set(0));

        world.get_mut::<floptle_core::Name>(other).unwrap().0 = "Renamed".into();
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        assert_eq!(crate::host::FULL_SYNCS.with(|c| c.get()), 1, "a rename must rebuild the mirror once");
        assert_eq!(world.get::<Transform>(e).unwrap().translation.y, 1.0, "find() did not see the new name");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The source decides whether `params` is scanned for writes.** A
    /// script that never assigns into `params` is never scanned; one that does
    /// is scanned after every hook, and its write still lands in the ECS.
    #[test]
    fn only_a_script_that_writes_params_is_scanned_for_writes() {
        let dir = std::env::temp_dir().join(format!("floptle_pscan_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "reader", "defaults = { speed = 1 }\nfunction update(node, dt) node.y = params.speed end\n");
        write_script(&dir, "writer", "defaults = { speed = 1 }\nfunction update(node, dt) params.speed = params.speed + 1 end\n");

        let (mut world, e) = world_with_script("reader");
        let mut host = ScriptHost::new();
        crate::host::PARAMS_SCANS.with(|c| c.set(0));
        for i in 0..5 {
            host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
        }
        assert_eq!(crate::host::PARAMS_SCANS.with(|c| c.get()), 0, "a reader was scanned");
        assert_eq!(world.get::<Transform>(e).unwrap().translation.y, 1.0);

        let (mut world, e) = world_with_script("writer");
        let mut host = ScriptHost::new();
        crate::host::PARAMS_SCANS.with(|c| c.set(0));
        for i in 0..3 {
            host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
        }
        assert!(crate::host::PARAMS_SCANS.with(|c| c.get()) >= 3, "a writer must be scanned each hook");
        let seeded = &world.get::<Scripts>(e).unwrap().0[0].params;
        assert!(
            seeded.iter().any(|(k, v)| k == "speed" && *v > 1.0),
            "the write never reached the ECS: {seeded:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The textual test errs toward "writes": every way a script could reach
    /// `params` without an obvious assignment counts, so a write can never be
    /// missed; only the plainly read-only shapes are exempt.
    #[test]
    fn the_params_write_test_is_conservative() {
        use crate::source_writes_params as w;
        assert!(w("params.speed = 2"));
        assert!(w("params[\"speed\"] = 2"));
        assert!(w("params[k] = v"));
        assert!(w("  params.x  =  1"));
        assert!(w("local p = params\np.x = 1"), "an alias is a possible write");
        assert!(w("tune(params)"), "handing it to a function is a possible write");
        assert!(w("t = { params }"), "storing it is a possible write");
        assert!(!w("node.y = params.speed"));
        assert!(!w("if params.speed == 2 then end"));
        assert!(!w("local s = params.speed * 2"));
        assert!(!w("print(params.name)"), "a field read passed to a call is a read");
        assert!(!w("myparams.x = 1"), "a longer identifier is not params");
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
    /// `floptle/0117`: the mirror now REUSES a tilemap's buffer instead of
    /// reallocating it every sync. The whole risk in that is staleness — a map
    /// that changed must still read as changed, on the very next frame — so this
    /// writes through the handle, steps frames, and reads back.
    #[test]
    fn a_reused_tilemap_mirror_still_sees_the_map_change() {
        let dir = std::env::temp_dir().join(format!("floptle_tm_reuse_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "tick",
            "\
function start(node)
  node:setTilemap{ cols = 4, rows = 4, tile = 1.0 }
  frame = 0
  stale = 0
end
function update(node, dt)
  local tm = node:tilemap()
  local got = tm:get(1, 1)
  -- The very first update runs before setTilemap has been applied, so there is
  -- nothing to read yet. From then on, what we read must be exactly what we
  -- wrote last frame — a reused buffer that was not refreshed would hand back
  -- an older number.
  if frame > 0 and got ~= frame then stale = stale + 1 end
  frame = frame + 1
  tm:set(1, 1, frame)
  tm:set(0, 0, stale)
end
",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "tick".into(),
                enabled: true,
                params: vec![],
                refs: vec![],
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        for _ in 0..4 {
            host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        }
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let Some(Matter::Tilemap { data, .. }) = world.get::<Matter>(e) else {
            panic!("no tilemap")
        };
        assert_eq!(
            floptle_core::tile_index(data[0]),
            0,
            "a frame read a stale grid — the reused buffer was not refreshed"
        );
        // Four runs, the first of which only creates the map: three writes land.
        assert_eq!(
            floptle_core::tile_index(data[5]),
            4,
            "the last write did not reach the ECS"
        );
    }

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

    /// A script that crosses the VM's upvalue ceiling must be told what
    /// happened (`floptle/0086`) — **and where there is no ceiling it must
    /// simply run.**
    ///
    /// On LuaJIT the raw message is `…:3669: function at line 2864 has more
    /// than 60 upvalues` — it names the END of the offending function rather
    /// than the reference that tipped it over, never says a limit exists, and
    /// arrives from the loader, so the script does not run at all.
    /// `vessel_controller` hit this twice, a release apart, on mechanical edits.
    ///
    /// Luau has no such ceiling (ADR-0028; `tests/vm_dialect.rs` measures it
    /// rather than quoting it), so the same 70-upvalue file that cost two
    /// releases there loads and runs here. That is a real difference between
    /// the two VMs and this test states it in both directions, because a
    /// `#[cfg]`-skipped test asserts nothing about the VM that skipped it.
    #[test]
    fn crossing_the_upvalue_ceiling_names_the_script_the_limit_and_the_fix() {
        let dir = std::env::temp_dir().join(format!("floptle_upvalue_over_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // 70 file-scope locals, and one function that closes over every one of
        // them: exactly the shape one more `local` produces in a long script.
        let mut src = String::new();
        for i in 0..70 {
            src.push_str(&format!("local v{i} = {i}\n"));
        }
        src.push_str("function update(node, dt)\n  local t = 0\n");
        for i in 0..70 {
            src.push_str(&format!("  t = t + v{i}\n"));
        }
        src.push_str("  node.y = t\nend\n");
        write_script(&dir, "huge", &src);

        let (mut world, _e) = world_with_script("huge");
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);

        let Some(limit) = crate::load_error::UPVALUE_LIMIT else {
            // No ceiling: the file the ledger is named after is just a file.
            assert!(
                host.errors().is_empty(),
                "this VM has no upvalue ceiling, so a 70-upvalue script must load: {:?}",
                host.errors()
            );
            let logs = host.drain_logs();
            assert!(
                !logs.iter().any(|l| l.level == LogLevel::Error),
                "…and must not report one either: {logs:?}"
            );
            return;
        };
        assert_eq!(limit, 60, "the message below quotes the limit; keep them together");

        let errs = host.errors().to_vec();
        let msg = errs.iter().find(|e| e.contains("huge")).unwrap_or_else(|| {
            panic!("the load failure must be reported: {errs:?}")
        });
        assert!(msg.contains("huge.lua"), "names the script: {msg}");
        assert!(msg.contains("60 upvalues"), "names the limit: {msg}");
        assert!(msg.contains("LuaJIT"), "names whose limit it is: {msg}");
        assert!(msg.contains("local s ="), "names the fix: {msg}");

        // …and ONCE on the Console, not once per frame. A load failure fails
        // every frame; sixty identical lines a second is how a Console feed
        // stops being read.
        let first = host.drain_logs();
        assert_eq!(
            first.iter().filter(|l| l.level == LogLevel::Error).count(),
            1,
            "one Console line for the load failure: {first:?}"
        );
        for _ in 0..5 {
            host.run(&mut world, &dir, 0.1, 0.1);
        }
        let later = host.drain_logs();
        assert!(
            !later.iter().any(|l| l.level == LogLevel::Error),
            "the same failure must not re-print every frame: {later:?}"
        );
        assert!(
            !host.errors().is_empty(),
            "…but the Scripting tab still lists it as currently broken"
        );
    }

    /// One edit from the wall, the engine says so — because the count is
    /// invisible from inside the editor, and crossing it costs the whole script.
    ///
    /// Where there is no wall (Luau — ADR-0028) the engine must say **nothing**.
    /// A warning about a limit that is not there is worse than silence: it sends
    /// somebody to restructure a working script for no reason.
    #[test]
    fn a_script_near_the_upvalue_ceiling_is_warned_before_it_crosses() {
        let dir = std::env::temp_dir().join(format!("floptle_upvalue_near_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut src = String::new();
        for i in 0..55 {
            src.push_str(&format!("local v{i} = {i}\n"));
        }
        src.push_str("function update(node, dt)\n  local t = 0\n");
        for i in 0..55 {
            src.push_str(&format!("  t = t + v{i}\n"));
        }
        src.push_str("  node.y = t\nend\n");
        write_script(&dir, "nearly", &src);

        let (mut world, _e) = world_with_script("nearly");
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);

        assert!(host.errors().is_empty(), "it still LOADS: {:?}", host.errors());
        let logs = host.drain_logs();
        if crate::load_error::UPVALUE_LIMIT.is_none() {
            assert!(
                !logs.iter().any(|l| l.msg.contains("upvalues")),
                "this VM has no upvalue ceiling, so nothing should warn about one: {logs:?}"
            );
            return;
        }
        let warn = logs
            .iter()
            .find(|l| l.level == LogLevel::Warn && l.msg.contains("upvalues"))
            .unwrap_or_else(|| panic!("expected an upvalue-pressure warning: {logs:?}"));
        assert!(warn.msg.contains("nearly.lua"), "{}", warn.msg);
        assert!(warn.msg.contains("55 file-scope locals"), "names the count: {}", warn.msg);
        assert!(warn.msg.contains("5 to go"), "names the headroom: {}", warn.msg);
        assert!(warn.msg.contains("before the script stops loading"), "{}", warn.msg);

        // Once per version of the file, not once a frame.
        for _ in 0..5 {
            host.run(&mut world, &dir, 0.1, 0.1);
        }
        assert!(
            !host.drain_logs().iter().any(|l| l.msg.contains("upvalues")),
            "the warning repeats every frame"
        );
    }

    /// A broken script and a script with no such export both read `nil` through
    /// a handle. Only one of them is a bug in the caller (`floptle/0086`).
    #[test]
    fn reading_from_a_script_that_failed_to_load_says_so() {
        let dir = std::env::temp_dir().join(format!("floptle_broken_read_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "radar", "function update(node, dt) end\nthis is not lua\n");
        write_script(
            &dir,
            "hud",
            "function update(node, dt)\n  local h = findScript('radar')\n  if h then local _ = h.target end\nend\n",
        );

        let mut world = World::default();
        for kind in ["radar", "hud"] {
            let e = world.spawn();
            world.insert(e, Transform::IDENTITY);
            world.insert(e, Scripts(vec![floptle_core::ScriptInst {
                kind: kind.into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]));
        }
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        host.run(&mut world, &dir, 0.1, 0.1);

        let logs = host.drain_logs();
        let told = logs
            .iter()
            .find(|l| l.msg.contains("`radar` did not load"))
            .unwrap_or_else(|| panic!("the reader was never told radar is broken: {logs:?}"));
        assert!(told.msg.contains("target"), "names the key it could not answer: {}", told.msg);
        assert!(
            told.msg.contains("not a missing export"),
            "the whole point is the distinction: {}",
            told.msg
        );
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

    /// Three lines of Lua, and the node walks across the level.
    ///
    /// This is the shape the whole agent layer exists to make possible, so it is
    /// worth pinning end to end rather than only in the crate that does the
    /// walking: a script that says `moveTo` and never touches a position, and a
    /// node that arrives anyway.
    #[test]
    fn an_agent_ordered_from_a_script_walks_the_node_there() {
        let dir = std::env::temp_dir().join("floptle_script_test_navagent");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "unit",
            "function start(node)\n\
             \x20 agent = nav.agent(node, { speed = 6, arrive = 0.4 })\n\
             \x20 agent:moveTo(vec3(9, 0, 9))\n\
             end\n\
             function update(node, dt)\n\
             \x20 if agent.arrived then arrived = true end\n\
             end\n",
        );

        // A plain 12x12 floor, baked for a small character.
        let floor = [
            floptle_nav::Tri::new([0.0, 0.0, 0.0], [12.0, 0.0, 0.0], [0.0, 0.0, 12.0]),
            floptle_nav::Tri::new([12.0, 0.0, 0.0], [12.0, 0.0, 12.0], [0.0, 0.0, 12.0]),
        ];
        let mesh = floptle_nav::bake(
            &floor,
            &floptle_nav::NavSettings { agent_radius: 0.3, cell_size: 0.15, ..Default::default() },
        )
        .expect("this floor bakes");

        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::from_translation(floptle_core::math::DVec3::new(1.5, 0.0, 1.5)));
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "unit".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );

        let mut host = ScriptHost::new();
        host.set_nav_mesh(Some(mesh));
        for _ in 0..400 {
            host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        }
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

        let at = world.get::<Transform>(e).unwrap().translation;
        assert!(
            (at.x - 9.0).abs() < 0.6 && (at.z - 9.0).abs() < 0.6,
            "the node should have walked to (9, 9): {at:?}"
        );
    }

    /// `agent:teleport` puts the NODE there — the host used to read the scene
    /// position straight back over it every frame, which turned a documented
    /// teleport into a `stop()` that moved nothing.
    #[test]
    fn an_agent_teleported_from_a_script_moves_the_node() {
        let dir = std::env::temp_dir().join("floptle_script_test_navteleport");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "porter",
            "function start(node)\n\
             \x20 agent = nav.agent(node)\n\
             \x20 agent:teleport(vec3(10, 0, 10))\n\
             end\n",
        );

        let floor = [
            floptle_nav::Tri::new([0.0, 0.0, 0.0], [12.0, 0.0, 0.0], [0.0, 0.0, 12.0]),
            floptle_nav::Tri::new([12.0, 0.0, 0.0], [12.0, 0.0, 12.0], [0.0, 0.0, 12.0]),
        ];
        let mesh = floptle_nav::bake(
            &floor,
            &floptle_nav::NavSettings { agent_radius: 0.3, cell_size: 0.15, ..Default::default() },
        )
        .expect("this floor bakes");

        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::from_translation(floptle_core::math::DVec3::new(1.5, 0.0, 1.5)));
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "porter".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );

        let mut host = ScriptHost::new();
        host.set_nav_mesh(Some(mesh));
        for _ in 0..10 {
            host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        }
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let at = world.get::<Transform>(e).unwrap().translation;
        assert!(
            (at.x - 10.0).abs() < 0.3 && (at.z - 10.0).abs() < 0.3,
            "the node should be AT the teleport point, not still at the spawn: {at:?}"
        );
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

    /// **A reference param follows the scene, not the wire.** `params.target`
    /// is resolved BY NAME, so a target that does not exist yet at the first
    /// frame must appear in `params` when it spawns, and vanish when it is
    /// renamed away — without the Inspector touching the wire. The per-hook
    /// rebuild of `params` used to give this for free; the fingerprinted
    /// rebuild has to earn it, and this is where it is watched.
    #[test]
    fn noderef_param_rebinds_when_the_target_appears_or_is_renamed_mid_play() {
        let dir = std::env::temp_dir().join(format!("floptle_noderef_live_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "seeker",
            "defaults = { target = noderef() }\n\
             function update(node, dt) node.x = params.target and 1 or 0 end\n",
        );
        let mut world = World::default();
        let driver = world.spawn();
        world.insert(driver, Transform::IDENTITY);
        world.insert(
            driver,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "seeker".into(),
                enabled: true,
                params: vec![],
                refs: vec![("target".into(), "Turret".into())],
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(world.get::<Transform>(driver).unwrap().translation.x, 0.0, "no Turret yet");

        // The target spawns mid-play, the way a streamed level or a prefab does.
        let turret = world.spawn();
        world.insert(turret, Transform::IDENTITY);
        world.insert(turret, floptle_core::Name("Turret".into()));
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        assert_eq!(
            world.get::<Transform>(driver).unwrap().translation.x,
            1.0,
            "a target that spawned after the first frame never reached params"
        );

        // …and a rename takes it away again.
        world.get_mut::<floptle_core::Name>(turret).unwrap().0 = "Decoy".into();
        host.run(&mut world, &dir, 1.0 / 60.0, 2.0 / 60.0);
        assert_eq!(
            world.get::<Transform>(driver).unwrap().translation.x,
            0.0,
            "a renamed target stayed bound under its old name"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **`print(v)` reads the same in both vec3 modes.** A `fast` vector is the
    /// VM's own value type, which the deep printer did not know and rendered
    /// as `<value>` — bare, and inside every table it was printed in.
    #[cfg(feature = "vm-luau")]
    #[test]
    fn a_fast_vec3_prints_as_a_vec3_and_not_as_a_value() {
        let dir = std::env::temp_dir().join(format!("floptle_fast_print_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "printer",
            "function update(node, dt)\n\
               print(vec3(1, 2, 3))\n\
               print({ p = vec3(4, 5, 6) })\n\
             end\n",
        );
        let (mut world, _e) = world_with_script("printer");
        let mut host = ScriptHost::new();
        host.set_vec3_mode(crate::Vec3Mode::Fast).expect("fast is available on this build");
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let logs = host.drain_logs();
        let msgs: Vec<&str> = logs.iter().map(|l| l.msg.as_str()).collect();
        assert!(msgs.iter().any(|m| m.contains("vec3(1, 2, 3)")), "bare vector: {msgs:?}");
        assert!(msgs.iter().any(|m| m.contains("vec3(4, 5, 6)")), "vector in a table: {msgs:?}");
        assert!(!msgs.iter().any(|m| m.contains("<value>")), "printed as <value>: {msgs:?}");
        let _ = std::fs::remove_dir_all(&dir);
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

    /// `floptle/0118`: the sky's uniforms are a THIRD place, and until this they
    /// were the only shader in the engine a script could not talk to. A
    /// procedural sky that can only be a function of `time` runs its story on a
    /// clock — the reported case was a cutscene sky whose city was revealed in
    /// the middle of whatever sentence the reader happened to be on.
    #[test]
    fn set_shader_param_reaches_the_sky() {
        let dir = std::env::temp_dir().join("floptle_script_test_sky_param");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "story",
            concat!(
                "function update(node, dt)\n",
                "  local sky = find(\"Skybox\")\n",
                "  sky:setShaderParam(\"burn\", 0.75)\n",
                "end\n",
            ),
        );
        let mut world = World::default();
        let sky = world.spawn();
        world.insert(sky, Transform::IDENTITY);
        world.insert(sky, floptle_core::Name("Skybox".into()));
        world.insert(
            sky,
            Matter::Skybox {
                color: [0.0; 3],
                size: 1000.0,
                texture: None,
                tint: [1.0; 3],
                shader: Some("shaders/ashfall.flsl".into()),
                shader_params: Default::default(),
            },
        );
        // A sky node that ALSO carries a material: the write must still go where
        // the sky pipeline reads, not into the material nobody draws.
        world.insert(sky, Material { shader: Some("shaders/x.flsl".into()), ..Default::default() });
        let driver = world.spawn();
        world.insert(driver, Transform::IDENTITY);
        world.insert(
            driver,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "story".into(),
                enabled: true,
                params: vec![],
                refs: vec![],
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let Some(Matter::Skybox { shader_params, .. }) = world.get::<Matter>(sky) else {
            panic!("the sky lost its matter")
        };
        assert_eq!(shader_params.get("burn"), Some(&[0.75, 0.0, 0.0, 0.0]));
        assert!(
            world.get::<Material>(sky).unwrap().shader_params.is_empty(),
            "the write went to the material instead of the sky"
        );
    }

    /// `floptle/0109` + `floptle/0113`: sorting layers and 2D lighting shipped
    /// with no script access at all, which rules out the ordinary 2D moves — a
    /// character stepping behind a counter, a torch that stops lighting the
    /// background. A misspelled enum has to NAME the accepted set rather than
    /// quietly meaning `auto` (`floptle/0072`).
    #[test]
    fn a_script_drives_sorting_and_2d_lighting() {
        let dir = std::env::temp_dir().join("floptle_script_test_sort2d");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "stack",
            concat!(
                "function start(node)\n",
                "  node:setSorting{ layer = \"Characters\", order = 3 }\n",
                "  node:setLighting2D{ mode = \"2d\", blocks = \"on\" }\n",
                "  local torch = find(\"Torch\")\n",
                "  torch:setLighting2D{ mode = \"2d\", layers = { \"Characters\" } }\n",
                "  ok, err = pcall(function() torch:setLighting2D{ mode = \"flat-ish\" } end)\n",
                "end\n",
            ),
        );
        let mut world = World::default();
        let hero = world.spawn();
        world.insert(hero, Transform::IDENTITY);
        world.insert(
            hero,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "stack".into(),
                enabled: true,
                params: vec![],
                refs: vec![],
                strs: Vec::new(),
            }]),
        );
        let torch = world.spawn();
        world.insert(torch, Transform::IDENTITY);
        world.insert(torch, floptle_core::Name("Torch".into()));
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

        let s = world.get::<floptle_core::Sorting>(hero).expect("sorting was not set");
        assert_eq!((s.layer.as_str(), s.order), ("Characters", 3));
        let lit = world.get::<floptle_core::Lighting2D>(hero).expect("no Lighting2D");
        assert_eq!(lit.mode, floptle_core::Lit2D::Yes);
        assert!(lit.layers.is_empty(), "a receiver names no layers");
        assert_eq!(
            world.get::<floptle_core::Shadow2D>(hero).map(|c| c.0),
            Some(floptle_core::Cast2D::Yes)
        );
        let torch_lit = world.get::<floptle_core::Lighting2D>(torch).expect("no Lighting2D");
        assert_eq!(torch_lit.layers, vec!["Characters".to_string()]);
        // …and the bad spelling raised rather than defaulting. `pcall` caught it,
        // so the run itself is still clean — which is the point: the script
        // author hears about the typo, the engine does not guess.
        assert_eq!(
            torch_lit.mode,
            floptle_core::Lit2D::Yes,
            "the refused write must not have changed anything"
        );
    }

    /// The sprite component as a HANDLE — `node:sprite()` — plus the call that
    /// used to fail in total silence.
    ///
    /// A 2D character flips on a turn, which is one boolean written every frame.
    /// The only route was `node:setSprite{ flipX = }`, and written positionally
    /// — `setSprite{ 8, 1, flipX }`, which is what somebody reaching for a
    /// six-argument call writes — every key read back as absent, so the call
    /// re-set the sprite to exactly what it already was: the print said `true`,
    /// the Inspector said nothing, and there was no error anywhere.
    #[test]
    fn a_script_reads_and_writes_the_sprite_component() {
        use floptle_core::Matter;

        let dir = std::env::temp_dir().join("floptle_script_test_sprite_handle");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "hero",
            concat!(
                "function start(node)\n",
                "  local sp = node:sprite()\n",
                "  sp.flipX = true\n",
                "  sp.cell = 4\n",
                "  sp.pivotY = 0\n",
                // Read-your-writes: the queue applies after the pass, so a
                // handle that could only see the mirror would answer with the
                // value from before the line above it.
                "  log('flipX reads ' .. tostring(sp.flipX))\n",
                "  log('cell reads ' .. tostring(sp.cell))\n",
                // The generic component route has to answer with a BOOLEAN:
                // 0 is truthy in Lua, so a number makes `if sp.flipY then`
                // always taken.
                "  local c = node:getcomponent('Sprite')\n",
                "  log('component flipY is a ' .. type(c.flipY))\n",
                // And the positional call raises instead of doing nothing.
                "  local ok, err = pcall(function() node:setSprite{ 8, 1, true } end)\n",
                "  log('positional: ' .. tostring(ok) .. ' ' .. tostring(err))\n",
                "end\n",
            ),
        );

        let mut world = World::default();
        let hero = world.spawn();
        world.insert(hero, Transform::IDENTITY);
        world.insert(
            hero,
            Matter::Sprite {
                ppu: 32.0,
                size: 1.0,
                cell: 0,
                flip_x: false,
                flip_y: false,
                pivot: [0.5, 0.5],
            },
        );
        world.insert(
            hero,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "hero".into(),
                enabled: true,
                params: vec![],
                refs: vec![],
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

        let Some(Matter::Sprite { cell, flip_x, pivot, ppu, .. }) = world.get::<Matter>(hero)
        else {
            panic!("the node stopped being a sprite")
        };
        assert!(*flip_x, "sp.flipX = true must reach the component the renderer reads");
        assert_eq!(*cell, 4, "sp.cell = 4 must reach the component");
        assert_eq!(pivot[1], 0.0, "sp.pivotY = 0 puts the origin at the feet");
        assert_eq!(pivot[0], 0.5, "…and leaves the axis it did not name alone");
        assert_eq!(*ppu, 32.0, "an untouched field keeps what the node had");

        let logs: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
        let said = |what: &str| {
            assert!(logs.iter().any(|l| l.contains(what)), "no log said {what:?}: {logs:?}");
        };
        said("flipX reads true");
        said("cell reads 4");
        said("component flipY is a boolean");
        said("positional: false");
        said("setSprite");
        assert!(
            logs.iter().any(|l| l.contains("positional") && l.contains("pivotX")),
            "the refusal must name the keys it does read: {logs:?}"
        );
    }

    /// `floptle/0118`, the other half: the post chain is typed knobs rather than
    /// a shader's uniforms, so it comes through the component route. A cutscene
    /// pushing a vignette is the reported want.
    #[test]
    fn script_drives_the_post_chain() {
        let dir = std::env::temp_dir().join("floptle_script_test_post");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "cut",
            concat!(
                "function update(node, dt)\n",
                "  local pp = find(\"Post\"):getcomponent(\"PostProcess\")\n",
                // Bloom is OFF in this scene, so this branch must not be taken.
                // If the field arrived as the number 0 instead of `false` it
                // would be — 0 is truthy in Lua — and the assertion below is
                // what catches that.
                "  if pp.bloom then pp.bloomIntensity = 2.5 end\n",
                "  pp.vignette = 1\n",
                "  pp.vignetteStrength = 0.8\n",
                "  pp.posterizeBands = -4\n",
                "end\n",
            ),
        );
        let mut world = World::default();
        let post = world.spawn();
        world.insert(post, Transform::IDENTITY);
        world.insert(post, floptle_core::Name("Post".into()));
        world.insert(post, Matter::default_post_process());
        let driver = world.spawn();
        world.insert(driver, Transform::IDENTITY);
        world.insert(
            driver,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "cut".into(),
                enabled: true,
                params: vec![],
                refs: vec![],
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let Some(Matter::PostProcess {
            bloom_intensity, vignette, vignette_strength, posterize_bands, ..
        }) = world.get::<Matter>(post)
        else {
            panic!("the post node lost its matter")
        };
        assert_eq!(
            *bloom_intensity, 0.7,
            "bloom is off, so `if pp.bloom` must be false — 0 would have been truthy"
        );
        assert!(*vignette);
        assert_eq!(*vignette_strength, 0.8);
        assert_eq!(*posterize_bands, 0, "a negative band count must floor at off, not wrap");
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
        world.insert(e, Matter::PointLight {
            color: [1.0, 1.0, 1.0],
            intensity: 2.0,
            range: 10.0,
            shape: Default::default(),
            shadows: false, spot_angle: floptle_core::OMNI_ANGLE, spot_softness: 0.25,
        });
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
        host.call_spawn_callback(
            &mut world,
            req.cb.expect("callback captured"),
            bullet.index(),
            &[bullet],
        );
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

    /// A sprite frame lands on whichever thing owns the cell.
    ///
    /// A `Matter::Sprite` carries its own `cell` and its Material's is unused,
    /// so writing the Material's for a Sprite node would set a number the
    /// Inspector shows and nothing draws — the worst kind of wrong, because it
    /// looks like it worked.
    #[test]
    fn a_sprite_frame_writes_the_cell_the_node_actually_reads() {
        use floptle_core::{Material, Matter};

        // A plane wearing a material: the Material's cell is the live one.
        let mut world = World::default();
        let plane = world.spawn();
        world.insert(plane, Transform::IDENTITY);
        world.insert(plane, Matter::Primitive { shape: floptle_core::Shape::Plane, color: [1.0; 3] });
        world.insert(plane, Material::default());
        crate::apply_sprite_frame(&mut world, plane, "art/hero.png", 8, 4, 5);
        let m = world.get::<Material>(plane).unwrap();
        assert_eq!(m.texture.as_deref(), Some("art/hero.png"));
        assert_eq!((m.sheet_cols, m.sheet_rows, m.cell), (8, 4, 5));

        // A Sprite node: the cell is on the Matter, and the Material's must be
        // left alone rather than set to a number nothing looks at.
        let sprite = world.spawn();
        world.insert(sprite, Transform::IDENTITY);
        world.insert(
            sprite,
            Matter::Sprite { ppu: 32.0, size: 1.0, cell: 0, flip_x: false, flip_y: false, pivot: [0.5, 0.5] },
        );
        world.insert(sprite, Material::default());
        crate::apply_sprite_frame(&mut world, sprite, "art/hero.png", 8, 4, 5);
        let Some(Matter::Sprite { cell, .. }) = world.get::<Matter>(sprite) else {
            panic!("not a sprite any more")
        };
        assert_eq!(*cell, 5, "the Sprite's own cell is the one that draws");
        let m = world.get::<Material>(sprite).unwrap();
        assert_eq!((m.sheet_cols, m.sheet_rows), (8, 4), "the grid is still the Material's");
        assert_eq!(m.cell, 0, "the Material's cell is unused here and must not be written");

        // …and reading it back gives what playing it put in — the two halves a
        // record-then-play round trip depends on.
        assert_eq!(
            crate::read_sprite_frame(&world, sprite),
            Some(("art/hero.png".to_string(), 8, 4, 5))
        );
        assert_eq!(
            crate::read_sprite_frame(&world, plane),
            Some(("art/hero.png".to_string(), 8, 4, 5))
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

    /// The tick-pose channel (`docs/multiplayer.md` §3).
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

    /// `net.random()` (`docs/multiplayer.md` §3): identical on
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

    /// The `replaying` gate (`docs/multiplayer.md` §4): a
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
        // The PRINT's own line must not be there. Matched with the level as
        // well as the text, because a runtime error now quotes the source line
        // it happened on (`crate::runtime_error`) — and in this script the
        // print and the `error()` share one line, so the error's own log
        // legitimately contains the word "quiet". Checking the text alone would
        // be asserting that the error message says less than it does.
        assert!(
            !logs.iter().any(|l| l.level != LogLevel::Error && l.msg.contains("quiet")),
            "…but the print must not: {logs:?}"
        );
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

    /// **A script with no frame hook still gets its params warnings.** The
    /// hook-less fast path (0.84.0) returns before the full setup, and the two
    /// `first`-only warnings lived inside the full setup — so a `fixedUpdate`-
    /// only controller consumed its first pass on the fast path and a tunable
    /// nobody reads went silent. The warning is about the SCENE's wiring, not
    /// about any hook, so it must fire whichever hooks the script has.
    #[test]
    fn a_fixed_update_only_script_is_warned_about_a_param_it_never_reads() {
        let dir = std::env::temp_dir().join(format!("floptle_fixed_only_params_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "mover",
            "defaults = { speed = 1 }\n\
             function fixedUpdate(node, dt) node.x = node.x + params.speed * dt end\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "mover".into(),
                enabled: true,
                params: vec![("speed".into(), 2.0), ("stale".into(), 3.0)],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        host.run_fixed(&mut world, 1.0 / 60.0, 1.0 / 60.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let logs = host.drain_logs();
        let msgs: Vec<&str> = logs.iter().map(|l| l.msg.as_str()).collect();
        assert!(
            msgs.iter().any(|m| m.contains("stale") && m.contains("never read")),
            "no unread-params warning for a fixedUpdate-only script: {msgs:?}"
        );
        assert!(
            !msgs.iter().any(|m| m.contains("speed") && m.contains("never read")),
            "a param the script declares was reported unread: {msgs:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A vec3 that cannot cross the wire is refused as a vec3.** Both
    /// backings used to fall through to the VM's type name — "userdata can't
    /// replicate" for `exact`, "vector can't replicate" for `fast` — neither of
    /// which names the thing the author wrote or what to send instead. A plain
    /// `{x=, y=, z=}` table is what to send, and it still crosses.
    #[test]
    fn a_vec3_that_cannot_replicate_is_named_and_the_fix_is_too() {
        // Both backings where the build has both; `fast` is Luau-only.
        let modes: &[Vec3Mode] =
            if cfg!(feature = "vm-luau") { &[Vec3Mode::Exact, Vec3Mode::Fast] } else { &[Vec3Mode::Exact] };
        for &mode in modes {
            let lua = mlua::Lua::new();
            crate::math_api::install(&lua).unwrap();
            crate::math_api::set_mode_checked(&lua, mode).unwrap();
            let v: mlua::Value = lua.load("return vec3(1, 2, 3)").eval().unwrap();
            let err = crate::net_api::lua_to_netvalue(&v, 0).expect_err("a vec3 is not a wire value");
            assert!(err.contains("vec3"), "{mode:?}: the refusal does not say vec3: {err}");
            assert!(err.contains("x, y, z"), "{mode:?}: the refusal does not name the fix: {err}");
            let t: mlua::Value = lua.load("return { x = 1, y = 2, z = 3 }").eval().unwrap();
            assert!(crate::net_api::lua_to_netvalue(&t, 0).is_ok(), "{mode:?}: the fix itself was refused");
        }
    }

    /// **A script that raises every frame reads its source once.** The runtime
    /// error rewriter quotes the offending line from the file, and it read the
    /// file per error — one read per instance per pass, sixty nodes on one
    /// broken script being 180 reads a frame. The text is kept with the source
    /// and dropped when the file changes (the mtime bump that already resets
    /// the generation), so the quoted line is never stale either.
    #[test]
    fn a_script_that_raises_every_frame_reads_its_source_once() {
        let dir = std::env::temp_dir().join(format!("floptle_source_once_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "faulty", "function update(node, dt)\n  node.postion.x = 1\nend\n");
        let (mut world, _e) = world_with_script("faulty");
        let mut host = ScriptHost::new();
        for i in 0..5 {
            host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
        }
        assert!(
            host.errors().iter().any(|e| e.contains("node.postion")),
            "the rewrite stopped quoting the line: {:?}",
            host.errors()
        );
        let reads = host.source_reads("faulty");
        assert_eq!(reads, 1, "five identical errors read the file {reads} times");

        // The file changes: the cached text must go with the old generation, so
        // the quoted line is the NEW line.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_script(&dir, "faulty", "function update(node, dt)\n  local a = 1\n  node.psotion.x = a\nend\n");
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        let f = std::fs::File::open(dir.join("faulty.lua")).unwrap();
        f.set_modified(later).unwrap();
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0);
        assert!(
            host.errors().iter().any(|e| e.contains("node.psotion")),
            "the quoted line came from the OLD file: {:?}",
            host.errors()
        );
        assert_eq!(host.source_reads("faulty"), 2, "the new version was not read exactly once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A seeded host rolls the same numbers every run.** `floptle run --seed`
    /// exists so two runs of a game that re-randomises its cast are comparable;
    /// it has to reach both `math.random` and the no-seed `rng()` form (which
    /// otherwise draws from the clock), and consecutive `rng()` calls must still
    /// be DIFFERENT streams — a seed that made every `rng()` the same stream
    /// would change the game rather than pin it.
    #[test]
    fn a_seeded_host_rolls_the_same_numbers_every_run() {
        let dir = std::env::temp_dir().join(format!("floptle_seeded_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "roller",
            "function update(node, dt)\n\
               local a, b = rng(), rng()\n\
               print(a:next(), b:next(), math.random(), a.seed, b.seed)\n\
             end\n",
        );
        let roll = |seed: Option<u32>| -> Vec<String> {
            let (mut world, _e) = world_with_script("roller");
            let mut host = ScriptHost::new();
            if let Some(s) = seed {
                host.set_seed(s);
            }
            for i in 0..3 {
                host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
            }
            assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
            host.drain_logs().into_iter().map(|l| l.msg).collect()
        };
        let first = roll(Some(7));
        assert_eq!(first.len(), 3, "{first:?}");
        assert_eq!(first, roll(Some(7)), "two runs with one seed disagreed");
        assert_ne!(first, roll(Some(8)), "two different seeds rolled the same run");
        // Every `rng()` in the run is its own stream: five seeds across the run,
        // no two alike.
        let seeds: Vec<&str> = first.iter().flat_map(|l| l.split_whitespace().skip(3)).collect();
        let mut uniq = seeds.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), seeds.len(), "seeded rng() streams repeated: {first:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Allocation is attributed to the script that made it.** `--alloc` gave
    /// a total; a game whose vector was 2% of its per-frame allocation had no
    /// way to see where the other 98% came from. Sampled around each hook call
    /// while the collector is stopped, which is the only time the difference in
    /// heap size means "what this hook allocated".
    #[test]
    fn allocation_is_attributed_to_the_script_that_made_it() {
        let dir = std::env::temp_dir().join(format!("floptle_alloc_by_script_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "hog",
            "function update(node, dt)\n  local t = {}\n  for i = 1, 2000 do t[i] = { i } end\nend\n",
        );
        write_script(&dir, "lean", "local n = 0\nfunction update(node, dt)\n  n = n + dt\nend\n");
        let mut world = World::default();
        for kind in ["hog", "lean"] {
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
        }
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        host.gc_collect();
        host.gc_stop();
        host.track_alloc(true);
        for i in 1..=10 {
            host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
        }
        let by = host.alloc_by_script();
        host.track_alloc(false);
        host.gc_restart();
        let of = |k: &str| by.iter().find(|(n, _)| n == k).map(|(_, b)| *b).unwrap_or(0);
        let (hog, lean) = (of("hog"), of("lean"));
        // 2000 one-element tables a frame for ten frames, each at least 16
        // bytes. Luau's heap counter moves in 16 KB allocator pages, so the
        // lean script may be charged a page or two that happened to fill on
        // its watch — the bar is "far below", not "nothing".
        assert!(hog > 10 * 2000 * 16, "hog allocated {hog} bytes over ten frames: {by:?}");
        assert!(lean < hog / 10, "lean ({lean}) is not far below hog ({hog}): {by:?}");
        assert_eq!(by.first().map(|(n, _)| n.as_str()), Some("hog"), "not sorted largest first: {by:?}");
        // Off means off: nothing accrues, and the readout is empty again.
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0);
        assert!(host.alloc_by_script().is_empty(), "tracking kept accruing after being turned off");
        let _ = std::fs::remove_dir_all(&dir);
    }

}
