//! # floptle-script: the Lua scripting host
//!
//! Game logic lives in `.lua` files under a project's `scripts/` folder,
//! attached to nodes: the [`floptle_core::Scripts`] component names which
//! scripts run, with per-instance `params`. [`ScriptHost`] embeds Luau through
//! `mlua` and drives them each frame.
//!
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
//! function fixedUpdate(node, dt)         -- every gameplay tick (constant dt)
//!   -- movement / gameplay / physics writes belong here (netcode cadence)
//! end
//! ```
//! The host hands each call a mutable `node` table (`x/y/z`,
//! `scale`/`scale_x..z`, `yaw/pitch/roll` in radians) synced to the node's
//! [`Transform`] before the call and read back after, plus the globals
//! `params` (this instance's values), `time` (seconds since play started) and
//! `dt`. The Lua standard library is in scope; `log("...")` prints to the
//! engine console.
//!
//! Each `(node, script)` pair gets its own environment, so per-instance state
//! persists across frames, and the host hot-reloads a script when its file
//! changes on disk, re-running it in a fresh environment.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use floptle_core::time::SystemTime;

use floptle_core::transform::Transform;
use floptle_core::{Entity, Material};
use mlua::{Lua, RegistryKey, Table};

/// Queued `node:setShaderParam(...)` writes: (entity index, which material,
/// uniform name, vec4 lanes). The material is `None` for the node's own — the
/// UI element, sky, post chain or `Material` component, as it always was — or
/// `Some(part)` for one part's override under `ObjectMaterials`
/// (`node:material("Head#2"):setShaderParam(...)`).
type ShaderParamSets = Rc<RefCell<Vec<(u32, Option<String>, String, [f32; 4])>>>;
/// `node:setShaderTexture(slot, path)` writes, queued per frame: (entity, which
/// material, slot name, texture ref). The ref is a project-relative image path,
/// an `rt:` render target, or the empty string to clear the slot.
type ShaderTextureSets = Rc<RefCell<Vec<(u32, Option<String>, String, String)>>>;
/// A material's shader knobs as mirrored for read-back: its `shader_params`
/// and `shader_textures`. Keyed by component name (`Material`, `Material:<part>`)
/// under the entity, and present only for materials that carry any.
type ShaderState = (
    std::collections::BTreeMap<String, [f32; 4]>,
    std::collections::BTreeMap<String, String>,
);
/// `node:setScreenShader(name, on)` toggles, queued per frame: (entity, the
/// screen shader's file stem, on). Its own queue rather than a magic uniform
/// name, because a shader is free to declare a knob called `enabled` and the
/// two must not mean the same thing.
type ScreenShaderToggles = Rc<RefCell<Vec<(u32, String, bool)>>>;

/// `(script key name, why the host keeps it)` — see [`ScriptHost::set_reserved_keys`].
type ReservedKeys = Rc<RefCell<Vec<(String, String)>>>;

/// The frame profile, shared between the driver, the Lua `perf` table and the
/// editor readout.
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

/// One screen-space rectangle a script queued via `draw.rect` /
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
    /// font.
    ///
    /// Project fonts append to the font stack and slot 0 is never theirs, so
    /// without this a game whose UI is a pixel font could not draw one
    /// immediate-mode string in it, and the symptom is not "wrong typeface"
    /// but text that reads as badly
    /// spaced, because a layout built on a monospace grid is being handed a
    /// proportional font.
    pub font: String,
}

/// One world-space filled triangle a script queued via `draw.tri` / `draw.cone`
/// / `draw.disc` this tick (immediate mode). Drawn by the runtime triangle
/// layer alongside the lines — solid gizmo geometry, world markers.
#[derive(Clone, Copy, Debug)]
pub struct DrawTri {
    pub a: [f64; 3],
    pub b: [f64; 3],
    pub c: [f64; 3],
    pub color: [f32; 4],
}

/// One world-space textured quad a script queued via `draw.quad` this tick
/// (immediate mode). `p` runs around the quad; `uv` is the texture rectangle
/// the corners map to (`u0, v0, u1, v1`: corner 0 at `(u0, v0)`, corner 1 at
/// `(u1, v0)`, corner 2 at `(u1, v1)`, corner 3 at `(u0, v1)`), so a ribbon
/// can put one slice of a streak image on each of its segments. Drawn
/// depth-tested and alpha-blended by the runtime triangle layer — a thing in
/// the world (a sword trail, a decal, a ground ring), not a gizmo over it.
#[derive(Clone, Debug, PartialEq)]
pub struct DrawQuad {
    pub p: [[f64; 3]; 4],
    pub uv: [f32; 4],
    pub color: [f32; 4],
    pub texture: String,
}

/// Queued `node:getcomponent(name).field = value` writes: (entity index,
/// component, field) → value, flushed to the ECS after `run`.
///
/// Determinism invariant (`docs/multiplayer.md` §3): the
/// host's `HashMap`/`HashSet` state is only ever *iterated* where order cannot
/// change simulation results — each queued write lands on a distinct key
/// (entity/component/field), scripts themselves run in ECS insertion order
/// (a `Vec` snapshot), and the `input` sets are lookup-only. Keep it that way:
/// if a future queue's application order can affect the sim, use a `Vec` or
/// sort before applying — netcode prediction replays depend on same-inputs →
/// same-results.
type ComponentWrites = Rc<RefCell<HashMap<(u32, String, String), f64>>>;
/// The colour-valued twin of [`ComponentWrites`] (`e.fill = color(...)`). A
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
/// A list rather than a single slot, because additive loads compose: a level
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
    /// `environment` is `{ environment = true }`: the layer owns the world's
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
/// The owner is the listening script, not the element — which is the whole
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
/// before the scripts run — what `ui.clicked(el)` and `ui.events()` read.
///
/// The same list the engine dispatches hooks from afterwards, published early
/// so a script that would rather ask than be called back gets this frame's
/// answer rather than last frame's.
type UiFrameEvents = Rc<RefCell<Vec<(u32, String)>>>;

pub mod app_api;
mod account_api;
pub mod budget;
mod api;
mod audio_api;
mod env;
mod host;
mod http_api;
mod preload_api;
pub use http_api::{browser_url, open_in_browser};
pub mod http_policy;
pub use http_policy::HttpPolicy;
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
/// either backing, a `vec2` (z = 0), a node handle, or a `{x=, y=, z=}` table.
///
/// The public read path. With two backings a caller outside this crate cannot
/// `borrow::<LuaVec3>()` and be right, and the failure is silent in the worst
/// way:
/// `AnyUserData::borrow` is bounded on `'static` and not on `UserData`, so a
/// borrow of the wrong type still compiles and merely never matches. Ask here
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
mod voice_api;
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
    effective_cell, mirror_component_strings, mirror_shader_state, read_sprite_frame,
    set_sprite_cell, mirror_component_colors, mirror_components, HANDLE_KEYS,
};
pub use input_api::{SharedDomain, SharedInput};
pub use voice_api::{VoiceCmd, VoiceOpts, VoiceState};
pub use net_api::{
    input_to_net, net_aim, net_to_input, NetCmd, NetRoleState, NetState, PeerIdentity,
    RewindScope, RollbackInfo,
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
    /// Keys that went down this frame (edge).
    pub keys_pressed: std::collections::HashSet<String>,
    /// Keys that went up this frame (edge).
    pub keys_released: std::collections::HashSet<String>,
    /// The characters entered this frame, resolved by the OS keyboard layout,
    /// with a paste folded in.
    ///
    /// Not the same question as `keys_pressed`: that one is physical (`"q"` is
    /// the key where Q sits on a qwerty board, which types `a` on azerty), and
    /// this is what the player meant to write. Polling keys to build a string
    /// gets the alphabet wrong for anyone whose keyboard isn't yours.
    pub typed: String,
    pub mouse: (f32, f32),
    pub mouse_delta: (f32, f32),
    pub scroll: f32,
    pub buttons_down: [bool; 3],
    pub buttons_pressed: [bool; 3],
    /// The active camera's world (yaw, pitch), captured with the snapshot —
    /// `input.aimYaw()`/`aimPitch()`. This makes camera-relative movement
    /// deterministic under prediction: the view direction rides the input
    /// command, so the server and any replay use exactly the angle the player
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
    /// `generation` drops it). Only a script that has raised is resident: a
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
    /// on a scene — reached as a panic.
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
    /// Whether this script's source ever assigns into `params` (`params.x =`,
    /// `params["x"] =`, `params[k] =`). Read once when the chunk is built.
    /// A script that never writes cannot have written, so the per-hook scan of
    /// the whole `params` table — a `String` per key per call — is skipped for
    /// it. Most scripts never write: three of Forgery's fifty-one do.
    ///
    /// A textual test, and conservative in the right direction: anything that
    /// looks like a write counts as one, and a script that reaches `params`
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
/// for one that has them, because a ref is resolved by name and has to follow
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
    /// This frame's solved UI element rects in window physical pixels (entity
    /// index → [x, y, w, h]); `node:uiRect()` reads it so scripts can hit-test
    /// the mouse against a panel's actual rendered position instead of guessing
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
    /// texture write is a rebind, not a buffer write — the two cost different
    /// things and the driver treats them differently.
    shader_texture_sets: ShaderTextureSets,
    screen_shader_toggles: ScreenShaderToggles,
    /// The physics colliders for this frame, so `raycast(...)` works inside a script. The
    /// editor lends the sim's colliders before running scripts and takes them back after.
    colliders: Rc<RefCell<Vec<floptle_physics::AnchoredCollider>>>,
    /// Raycastable dynamic-body hulls for this frame ([`Sim::body_hulls`] copies —
    /// players, crates), fed alongside the colliders so `raycast(...)` can hit
    /// bodies and name the node it hit (`hit.node`). `net.rewind` re-poses these
    /// for lag-compensated combat queries (`docs/multiplayer.md` §7).
    hulls: Rc<RefCell<Vec<floptle_physics::BodyHull>>>,
    /// World position of the sim's local origin. Scripts speak world
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
    /// frame): body names whose terrain should be resident regardless of any
    /// gameplay anchor's distance — the map warms its focused planet while
    /// open. A warmed body loads if cold and never evicts.
    terrain_warm: Rc<RefCell<Vec<String>>>,
    /// The editor's answer to `terrain.busy()`: true while the background
    /// terrain worker has a field generating or streaming in. Published each
    /// frame so a game that builds its world on demand can wait its turn
    /// instead of queueing new worlds behind the ground someone stands on.
    terrain_busy: Rc<std::cell::Cell<bool>>,
    /// `terrain.flush()` — write every dirty resident field to the save slot
    /// now (checkpoints, exit-to-menu). One-shot flag drained per frame.
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
    /// Script kinds that failed to load — shared with the reference layer, which
    /// reads it to tell a broken script apart from a missing export. See the
    /// `Shared` copy.
    broken: Rc<RefCell<std::collections::HashSet<String>>>,
    broken_read_warned: Rc<RefCell<std::collections::HashSet<(String, String)>>>,
    /// The two remaining once-ever diagnostic sets, held here only so that
    /// [`ScriptHost::reset_diagnostics`] can empty them at the start of a run.
    /// See the `Shared` copies for what each one suppresses.
    find_scope_warned: Rc<RefCell<std::collections::HashSet<String>>>,
    miss_warned: Rc<RefCell<std::collections::HashSet<String>>>,
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
    /// only by the global `run_fixed` — never by `run_fixed_for`/replays, or
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
    /// Keys the host answers itself, so a script polling one is never going to
    /// see it — `(script name, why)`, filled by the driver
    /// ([`ScriptHost::set_reserved_keys`]). The editor reserves Play/Pause/Step;
    /// a headless harness reserves nothing.
    ///
    /// It exists so the first poll of such a key writes a Console line instead
    /// of returning `false` forever: unavailable looks exactly like not
    /// pressed, and a game with an inventory bound to Tab would hear about it
    /// from a player rather than from a test.
    reserved_keys: ReservedKeys,
    /// Where this frame's time went, per subsystem and per script.
    /// Written by the driver and by [`ScriptHost::run_pass`],
    /// read by the editor readout and by the Lua `perf` table — one structure, so
    /// a game's own budget assertion and the number on screen cannot disagree.
    ///
    /// Off by default and free while off. It exists because "the engine is slow"
    /// was the only report a game could make, and four such reports turned out to
    /// be four different numbers the game could have read itself.
    profile: SharedProfile,
    /// The player's accessibility settings, shared with `access.*` in Lua and
    /// read by the driver each frame.
    access: crate::access_api::SharedAccess,
    /// Captions `caption(...)` asked for, drained by the driver and drawn by the
    /// engine so every game gets the same readable placement.
    caption_queue: crate::access_api::CaptionQueue,
    /// What the game currently is — title, engine version, and the video
    /// settings a player can change. Pushed by the driver, read by `app.*`.
    app_info: crate::app_api::SharedAppInfo,
    /// What `app.*` asked the driver to change or do this frame. Every one of
    /// them touches something only the driver owns — the swap chain, a GPU
    /// target, the event loop — so none can be done from inside a Lua call.
    app_requests: crate::app_api::SharedAppRequests,
    /// `params.X = value` writes queued this pass — (entity, script kind, key,
    /// value). Flushed to the node's stored `ScriptInst` params so tunables are
    /// two-way: the write persists across frames and shows live in the
    /// Inspector (and reverts on Stop like every play-mode change). Numbers
    /// and strings; only declared tunables persist (a key in `defaults` or the
    /// stored params).
    param_writes: RefCell<Vec<(u32, String, String, ParamWrite)>>,
    /// Pending `scene.load(...)` / `scene.unload(...)` requests. The driver
    /// drains them and performs each between frames — locally when
    /// offline/hosting, over the wire to every client in a session.
    scene_request: SceneQueue,
    /// `scene.onLoaded(fn)` subscriptions, as `(owner entity, callback)`. The
    /// owner is recorded so a subscription dies with the script that made it —
    /// otherwise a swap would leave every old scene's loading screen listening.
    /// A persistent node's subscription survives, which is the entire point:
    /// something has to outlive the load to be told about it.
    scene_loaded: Rc<RefCell<Vec<(u32, mlua::RegistryKey)>>>,
    /// Every WaterVolume in the scene, refreshed by the driver before scripts
    /// run — what `water.depthAt` / `water.at` read.
    water_volumes: Rc<RefCell<Vec<water_api::WaterInfo>>>,
    /// `water.setFrozen(node, on)` requests, drained by the driver.
    water_freeze: Rc<RefCell<Vec<(u32, bool)>>>,
    /// Scatter sources scripts declared — resolved into
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
    /// This tick's `draw.quad(...)` textured quads (immediate mode).
    draw_quads: Rc<RefCell<Vec<DrawQuad>>>,
    draw_rects: Rc<RefCell<Vec<DrawRect>>>,
    draw_texts: Rc<RefCell<Vec<DrawText>>>,
    /// The `http.*` bridge: callbacks waiting on a reply, the caps, and the
    /// session generation that keeps a stale reply out of a fresh Play.
    http: Rc<RefCell<http_api::HttpState>>,
    /// How long one pass into Lua may run — see [`budget`].
    budget: Rc<budget::Budget>,
    /// Scripts that ran past the budget. Not called again until their file
    /// changes: a loop without an exit would otherwise freeze every frame for
    /// the whole budget, forever.
    stopped: std::collections::HashSet<String>,
    /// `print`/`log` lines refused this frame past the per-frame cap — see
    /// `host::MAX_CONSOLE_LINES_PER_FRAME`.
    dropped_lines: Rc<std::cell::Cell<usize>>,
    /// The `account.*` bridge: the player's Foverse account and the Cloud calls
    /// waiting on a reply. Built lazily inside — a project that never signs
    /// anybody in never touches the OS keyring.
    account: Rc<RefCell<account_api::AccountState>>,
    /// `assets.preload`: the models scripts asked for ahead of time, and the
    /// callbacks waiting on them.
    preloads: Rc<RefCell<preload_api::Preloads>>,
    /// True while the tick pass is running — `http.*` warns once when called
    /// from there, because nothing about a reply's timing can be replayed.
    http_in_fixed: Rc<std::cell::Cell<bool>>,
    /// The `steam.*` bridge's backend — `NullPlatform` unless a caller has
    /// explicitly decided this session is the game and called
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
    /// The `voice.*` bridge: queued voice commands + mirrored microphone and
    /// speaker state. Separate from `net` because voice lives
    /// with the session, not the scene — a scene swap must not reset it.
    voice: voice_api::SharedVoice,
    /// Per-(entity, script) `synced` store tables (the raw values behind the
    /// proxy scripts see) — the host collects them for the server session and
    /// writes received updates into them on clients. Shared (Rc) with the
    /// `net.rewind` closure, which swaps historical values in around a
    /// lag-compensated handler and restores after.
    synced_stores: Rc<RefCell<HashMap<(u32, String), Table>>>,
    /// (eid, script, var) combos already warned about failing the replication
    /// guardrails — so a hot loop doesn't spam the Console every tick.
    synced_warned: std::collections::HashSet<(u32, String, String)>,
    /// (eid, material, knob) shader writes already reported as having nothing
    /// to land on — a part with no override, an override wearing no shader —
    /// so a `setShaderParam` in `update` says so once, not every tick.
    shader_warned: std::collections::HashSet<(u32, String, String)>,
    /// `(script kind, param name)` already reported as stored-but-unread this
    /// session, so a param carried on eighteen instances of the same script is
    /// one Console line rather than eighteen.
    param_warned: std::collections::HashSet<(String, String)>,
    /// Bytes of Lua heap allocated inside each script kind's hook calls while
    /// `alloc_track` is on — see [`ScriptHost::track_alloc`].
    alloc_by_kind: RefCell<HashMap<String, u64>>,
    alloc_track: std::cell::Cell<bool>,
    /// `(script kind, key)` combos already reported as shadowing a `findScript`
    /// handle's own key — one line per script per session, not
    /// one per instance.
    handle_key_warned: std::collections::HashSet<(String, String)>,
    /// `(script kind, generation)` whose load failure has already been put on the
    /// Console. A broken script is re-reported into `errors` every frame (the
    /// Scripting tab is a live list), but the Console line is once per version
    /// of the file — otherwise one unloadable script buries every other message
    /// in the feed at sixty lines a second.
    load_failure_reported: std::collections::HashSet<(String, u64)>,
    /// `(script kind, generation)` already warned as *approaching* LuaJIT's
    /// upvalue ceiling. Same once-per-version rule, and it clears on edit — so
    /// the warning comes back the moment the file grows again.
    upvalue_warned: std::collections::HashSet<(String, u64)>,
    /// Entities whose scripts are skipped this session (a networked client
    /// doesn't run server-authoritative nodes' scripts — their state arrives
    /// in snapshots; docs/multiplayer.md §6). Set by the driver.
    script_skip: std::collections::HashSet<u32>,
    /// Entities skipped in the per-frame pass only: a predicted node's
    /// `update` re-runs on the gameplay tick (`run_frame_for`) so client and
    /// server integrate identically.
    frame_skip: std::collections::HashSet<u32>,
    /// Entities whose ticks a driver owns (the rollback driver): their
    /// `fixedUpdate` and `update` run from there, so the global passes skip
    /// them — but their `lateUpdate` does not run there and is not skipped
    /// here. Separate from `script_skip` because a driver-owned node is still
    /// locally simulated; only the scheduling moved.
    driver_skip: std::collections::HashSet<u32>,
    /// Set while the rollback driver is re-simulating ticks it already ran
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
/// Gating at the drain rather than inside each Lua closure is deliberate: a
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
    /// Aim every Beam track's endpoint at a world-space point — the editor
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
    /// name → first entity in scene order with that name: the O(1) index behind
    /// `find()` and node-reference params (no more linear scans per call).
    by_name: HashMap<String, u32>,
    parent: HashMap<u32, u32>,
    children: HashMap<u32, Vec<u32>>,
    /// Entity → the script kinds attached to it (for `node:getscript`).
    scripts: HashMap<u32, Vec<String>>,
    /// script kind → every entity carrying it, in scene order — the index behind
    /// `findScript` / `findScripts`.
    ///
    /// These are the calls a gameplay codebase makes most, because they are how
    /// one script reaches another and the alternative (an Inspector wire) does
    /// not exist for a singleton sixteen panels want, for "is any craft being
    /// flown", or for anything spawned at runtime. Walking the scene and
    /// string-comparing per node made the cost of asking scale with the scene:
    /// one real project issued 126 full-scene scans a frame, none of them
    /// carelessly written.
    ///
    /// Scene order is load-bearing — `findScript` returns the first, and call
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
    /// `tm:get` / `tm:size` / `tm:solid` without reaching into the world.
    tilemaps: HashMap<u32, TilemapMirror>,
    /// The project's loaded tilesets, keyed by their project-relative path.
    ///
    /// Lent by the host (`ScriptHost::set_tilesets`), the same way the layer table
    /// is: the script host does no file I/O of its own, so who owns the parse is
    /// unambiguous and a headless test can hand in a tileset without a project on
    /// disk. A path with no entry means the tileset failed to load or was never
    /// referenced — `tm:solid` then answers `false` rather than guessing, and the
    /// editor is the one that says so in the Console.
    tilesets: HashMap<String, floptle_tiles::TileSet>,
    /// Entities that are sprite batches, so `node:sprites()` can refuse a node
    /// that is not one instead of handing back a handle whose every draw is
    /// silently dropped.
    sprite_batches: std::collections::HashSet<u32>,
    /// Sprite nodes' own numbers, so `node:sprite()` can read them.
    ///
    /// `setSprite` shipped write-only, the same gap `sorting` above had: a
    /// character that flips on a turn has to ask which way it is facing, and a
    /// value you cannot read is one every caller ends up shadowing in a local —
    /// which is then the second copy that goes stale.
    ///
    /// Written by the per-frame sync and by every script-side write, so a read
    /// straight after an assignment answers with what was just assigned rather
    /// than with what the frame started as (the queue itself does not apply
    /// until after the pass).
    pub(crate) sprites: HashMap<u32, SpriteMirror>,
    /// What each node said about sorting, so `node:sorting()` can read it.
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
    /// UI images' current texture path (so a script can read `node.texture`,
    /// not just write it).
    ui_textures: HashMap<u32, String>,
    /// Nodes that carry an explicit `Visible` component (so a script can read
    /// `node.visible`; absent = visible by default).
    visible: HashMap<u32, bool>,
    /// Nodes carrying `floptle_core::Disabled` themselves (not inherited) — what
    /// `node.enabled` reads back. Inheritance is resolved by the engine, not mirrored.
    disabled: std::collections::HashSet<u32>,
    /// Nodes carrying `floptle_core::Persistent` themselves — what
    /// `node.persistent` reads back. Same rule as `disabled`: the subtree
    /// inheritance is the engine's to resolve, not the mirror's to duplicate.
    persistent: std::collections::HashSet<u32>,
    /// Nodes with an explicit `Layer` component, by layer name (absent =
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
    /// …and the string-valued half (`mat.texture`, `el.text`, `el.style`).
    ///
    /// Writable since strings landed, and readable by nothing: `mat.texture`
    /// answered nil however many times it had been set, so a script could not
    /// ask what a material was wearing — only tell it. Which makes the obvious
    /// swap ("put the shirt on unless it is already on") impossible to write.
    component_strings: HashMap<u32, HashMap<String, HashMap<String, String>>>,
    /// …and each material's shader knobs — uniforms and texture slots — so a
    /// part handle's `:shaderParam("glow")` reads back what the part's
    /// override carries, the way `.color` does.
    shader_state: HashMap<u32, HashMap<String, ShaderState>>,
    /// Model asset path → the material slots it was imported with, lent by the
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
    /// full sync — see `ScriptHost::sync_scene`. `0` means never synced.
    synced_non_transform_rev: u64,
}

/// Whether a `find*` call may return switched-off nodes.
///
/// Enabled-only is the default, and it is the whole point: a node you switched
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
    /// only switched-off nodes — for a tool that manages the parked ones.
    Disabled,
}

impl FindScope {
    /// Every spelling the options table accepts, and the list an error prints.
    ///
    /// One list read by the parser and the message: a defaulted bad value is
    /// how `pin = "topCenter"` silently meant top-left.
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
    /// The mirror stores only each node's own `Disabled`, (the
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
    /// Spawn the prefab's root(s) as children of this entity (kept at the
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
    /// `node:setPrimitive(shape [, color])`. The shape is already parsed — the
    /// name was checked at the call, where a misspelling can still name a line.
    MatterPrimitive(floptle_core::Shape, [f64; 3]),
    /// `node:setTextSpans{...}` — per-stretch colours along this element's text.
    /// An empty list clears them back to one colour.
    TextSpans(Vec<floptle_ui::TextSpan>),
    /// `node:setGlyphOffsets{...}` — a draw-time displacement per character.
    /// Empty clears. Applied after layout, so it moves glyphs and never
    /// re-wraps the line they are in.
    GlyphOffsets(Vec<[f32; 2]>),
    /// `node:setTilemap{...}` — build (or re-shape) a 2D grid on this node.
    MatterTilemap {
        cols: u32,
        rows: u32,
        tile: f32,
        data: Vec<u32>,
        /// `None` keeps whatever the node already referenced — `setTilemap` is
        /// also how a script resizes a map, and dropping the tileset on a resize
        /// would silently un-solid the level.
        tileset: Option<String>,
    },
    /// `node:setSpriteBatch{ size = }` — the other half of the 2D pair. A
    /// game's sprite styles are data (one batch per material, one material per
    /// style), so the nodes that draw them have to be makeable from the same
    /// Lua that declares them, not authored one-by-one into a scene and kept in
    /// sync by nothing.
    MatterSpriteBatch { size: f32 },
    /// `node:setSorting{ layer =, order = }` — where a 2D node draws in the
    /// stack.
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
    /// Every axis is its own option. A pair collapsed into `[x, y]` at the
    /// binding, with `0.0` for the axis nobody mentioned, would let
    /// `setCamera2D{ maxY = 80 }` set `maxX` to zero and park the camera
    /// against a limit nobody wrote.
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
        /// pivotY = 0 }` must leave `pivotX` alone, and that call is the
        /// documented way to move a character's origin to its feet.
        pivot_x: Option<f32>,
        pivot_y: Option<f32>,
    },
    /// `node:setTint(color [, alpha])` — a multiplier over everything this node
    /// draws, or `node:setTint()` to clear it.
    ///
    /// Separate from `Material` because it is a different act: a Material says
    /// what a thing is made of and replaces the model's own materials, while a
    /// tint leaves all of that alone and multiplies over the result. Flashing a
    /// character red must not cost it its textures.
    ///
    /// Every lane is an `Option` and `None` means "leave this one as it was",
    /// because the lanes are independent and set at different times: a fighter
    /// asks for its ambient lift once when it spawns and rewrites its colour on
    /// every hit flash, and a `setTint(red)` that silently dropped the ambient
    /// would put the character back in the dark for the length of the flash.
    /// `clear` is the one thing that takes the whole component away.
    NodeTint {
        color: Option<[f32; 3]>,
        alpha: Option<f32>,
        rim: Option<[f32; 3]>,
        rim_strength: Option<f32>,
        ambient: Option<f32>,
        clear: bool,
    },
    /// `node:setLighting2D{ mode =, layers =, blocks = }` — the 2D lighting flag,
    /// the layers a light reaches, and whether this node blocks light.
    ///
    /// One call rather than three because they are one feature and a node uses
    /// one half of it: a light sets `mode` and `layers`, a receiver sets `mode`
    /// and `blocks`.
    MatterLighting2D {
        mode: Option<floptle_core::Lit2D>,
        layers: Option<Vec<String>>,
        blocks: Option<floptle_core::Cast2D>,
        /// The shaping half (`0125`): full brightness out to
        /// `inner`, the exponent of the ramp after it, and whether casters stop
        /// this light at all.
        inner: Option<f32>,
        falloff: Option<f32>,
        shadows: Option<bool>,
    },
    /// `node:setPointLight{ color =, intensity =, range = }`.
    ///
    /// Until this a script could write an existing light's fields but never make
    /// one, so the only way to have dynamic light was to author N of them into
    /// the scene and pool them — which is also how a game exhausted the
    /// sixteen-slot budget with lights that were switched off. Every field is
    /// optional and keeps what the node already had, so this is a create and an
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
    /// a live `rt:<name>` texture at a chosen size and refresh rate.
    ///
    /// Every field is an `Option` of a value the engine will act on, not a
    /// `(name, value)` pair: the table is validated at the call, where a
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
/// sufficient on its own: the object name addresses exactly one part but is
/// rewritten by import when a model repeats a name (`Torso` becomes `Torso#2`),
/// while the material name is the one on the model's own materials list and
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
    /// The one place both the clamps and the keep-what-you-had rule live: the
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
    /// Cross-node position writes onto entities that have a physics body —
    /// the driver teleports the body there (otherwise the physics writeback
    /// stomps the transform next frame and the write silently vanishes).
    body_pos_changes: Rc<RefCell<HashMap<u32, [f64; 3]>>>,
    /// This frame's `b:draw(...)` calls per sprite-batch entity.
    ///
    /// Immediate mode, like `draw.*` and `gizmo.*`: the list is taken every
    /// pass and becomes that node's whole set of sprites, so what you drew this
    /// frame is exactly what shows and there is no `clear()` anyone can forget.
    /// A retained list would leak for as long as the game ran.
    sprite_draws: Rc<RefCell<HashMap<u32, Vec<floptle_core::Sprite>>>>,
    /// `node:setShaderParam(...)` writes, drained by the editor per frame.
    shader_param_sets: ShaderParamSets,
    /// See [`ShaderTextureSets`]. Separate from the uniform queue because a
    /// texture write is a rebind, not a buffer write — the two cost different
    /// things and the driver treats them differently.
    shader_texture_sets: ShaderTextureSets,
    screen_shader_toggles: ScreenShaderToggles,
    /// (entity index, script kind) → that instance's live Lua environment, so a
    /// script handle can read its state, call its methods, and read its params.
    ///
    /// A `RegistryKey`, resolved to a `Table` at each use. It held the `Table`
    /// directly until a Table alive in Rust turned out to cost a slot on mlua's
    /// Auxiliary ref stack, which is bounded near 8,000 — so a scene of a few
    /// thousand scripted nodes exhausted it and the engine panicked, in the
    /// editor, where unsaved work lives. The registry is an
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
    /// the node's full new tag list, applied as a `Tags` component.
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
    /// Script kinds that failed to load this session. A broken script and a
    /// script with no such export both read `nil` through a handle, and the two
    /// want completely different fixes — so a read against a name in here says
    /// which one it is, once per `(script, key)`.
    broken: Rc<RefCell<std::collections::HashSet<String>>>,
    /// `(script kind, key)` combos already told they were reading from a broken
    /// script, so a handle polled every frame is one Console line.
    broken_read_warned: Rc<RefCell<std::collections::HashSet<(String, String)>>>,
    /// Names a `find*` came up empty on while a switched-off node of that name
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
    /// The body's world position at the start of this tick — what
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
    /// sky. A controller that can see the wall simply stops pushing into it.
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
    // Wasd or the screen edge, commandable units, and the mouse layer that
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

    /// Every shipped script must at least compile.
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

    /// Every controller/camera example must run — `start` and a few frames
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
mod tests;
