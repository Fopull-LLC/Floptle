//! Lua authoring support written into every project: the default scripts each
//! project ships with (ADR-0003), and the language-server files (EmmyLua
//! annotations + `.luarc.json`) that give external IDEs hover docs and
//! completion for the engine scripting API.

use std::path::Path;

/// The default Lua scripts every project ships with (ADR-0003): the engine's
/// built-in behaviors, now plain hot-reloadable Lua the user can read and edit.
pub(crate) const DEFAULT_SCRIPTS: &[(&str, &str)] = &[
    ("rotate.lua", include_str!("../../../assets/scripts/rotate.lua")),
    ("pulsate.lua", include_str!("../../../assets/scripts/pulsate.lua")),
    ("float.lua", include_str!("../../../assets/scripts/float.lua")),
    // Ready-made character setups: an FPS body-camera, and a third-person
    // pair (body controller + orbit camera with first-person zoom).
    ("first_person.lua", include_str!("../../../assets/scripts/first_person.lua")),
    ("third_person.lua", include_str!("../../../assets/scripts/third_person.lua")),
    (
        "third_person_camera.lua",
        include_str!("../../../assets/scripts/third_person_camera.lua"),
    ),
];

/// EmmyLua type annotations for the engine API, so an external Lua language server
/// (e.g. VSCode's Lua extension) gives hover docs + completion for `node`, `params`,
/// `time`, `dt`, the lifecycle hooks, etc. Written to `.floptle/library/`.
pub(crate) const LUA_ANNOTATIONS: &str = "\
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
---@field visible boolean Show / hide this node's geometry (Inspector eye toggle).
---@field height number Physics (capsule bodies): standing height - write a smaller value to crouch.
---@field getcomponent fun(self: Node, name: string): RigidBodyHandle|PointLightHandle|nil Live component handle (RigidBody / PointLight), nil if the node lacks it.
---@field particles fun(self: Node): ParticleSystemHandle The particle handle for this node's Particle System: play / stop / restart the effect and read its live state.

---A Rigidbody's live tunables (every Inspector field). Assign to change while playing;
---booleans may be written true/false and read back as 1/0.
---@class RigidBodyHandle
---@field friction number Surface friction 0..1 (0 = frictionless).
---@field restitution number Bounciness 0..1 (0 = no bounce).
---@field gravity number Gravity pull on this body (1/0; assign true/false).
---@field shape number Body shape: 0 = sphere, 1 = capsule, 2 = box.
---@field radius number Sphere/capsule radius.
---@field height number Capsule total height.
---@field half_x number Box half-extent X.
---@field half_y number Box half-extent Y.
---@field half_z number Box half-extent Z.
---@field lock_x number Freeze world X translation (1/0).
---@field lock_y number Freeze world Y translation (1/0).
---@field lock_z number Freeze world Z translation (1/0).
---@field lock_rot_x number Freeze rotation about X (1/0).
---@field lock_rot_y number Freeze rotation about Y (1/0).
---@field lock_rot_z number Freeze rotation about Z (1/0).

---A Point Light's live tunables.
---@class PointLightHandle
---@field intensity number Brightness multiplier.
---@field range number Reach in world units.
---@field r number Color red 0..1.
---@field g number Color green 0..1.
---@field b number Color blue 0..1.

---A node's Particle System, controlled from a script via `node:particles()`.
---Start/stop the effect at runtime and read whether it's playing.
---@class ParticleSystemHandle
---@field play fun(self: ParticleSystemHandle) Start emitting if idle (spawns a fresh instance).
---@field stop fun(self: ParticleSystemHandle) Stop + despawn — the live particles vanish.
---@field restart fun(self: ParticleSystemHandle) Re-spawn from t=0 (re-fire a one-shot burst).
---@field isPlaying fun(self: ParticleSystemHandle): boolean Is an instance emitting/ageing right now?
---@field alive fun(self: ParticleSystemHandle): number Live particle count across the effect's tracks.
---@field asset fun(self: ParticleSystemHandle): string|nil The effect asset key this node references.

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

---Spawn a one-shot particle effect at a world point — no node required. It plays
---once and despawns itself. Great for hits, pickups, footstep poofs.
---e.g. `spawnEffect(\"vfx/Explosion\", hit.x, hit.y, hit.z)`
---@param key string Effect asset key (project-relative, no `.vfx.ron`).
---@param x number
---@param y number
---@param z number
function spawnEffect(key, x, y, z) end

---Runs once when play begins (optional).
---@param node Node
function start(node) end

---Runs every frame while playing.
---@param node Node
---@param dt number Seconds since the last frame.
function update(node, dt) end

---Runs every GAMEPLAY TICK (60 Hz, constant dt) — put movement, gameplay, and
---physics writes here; keep cameras/cosmetics in `update`. This is the fixed,
---deterministic cadence physics steps at (and the one multiplayer prediction
---will replay), so tick code behaves the same at any frame rate.
---@param node Node
---@param dt number The constant tick delta (1/60 s by default).
function fixedUpdate(node, dt) end

---Multiplayer (docs/netcode-design.md). Mark nodes with the Networked component,
---declare synced vars with a top-level `replicated = { hp = 100 }` table (read/
---write them as `synced.hp` — the server owns them), handle remote calls with
---`onRpc = {}` + `function onRpc.name(args, sender) end`.
---@class Net
net = {}
---Become the authoritative host. With `port`, host a REAL session on UDP
---(QUIC) that other machines join with net.join(\"quic://ip:port\"); without
---one, the in-editor loopback harness.
---@param opts { maxPlayers: integer, port: integer }|nil
function net.host(opts) end
---Join a session: `\"quic://host:port\"` = a real server over the network,
---`\"local://\"` = the in-editor test harness. relay:// lobby codes arrive
---with floptle-relay.
---@param addr string
function net.join(addr) end
---Leave / end the session.
function net.leave() end
---This endpoint's role.
---@return \"offline\"|\"server\"|\"client\"
function net.role() end
---@return boolean
function net.isServer() end
---@return boolean
function net.isClient() end
---Connected client peer ids (server).
---@return integer[]
function net.peers() end
---Round-trip time in milliseconds.
---@param peer integer|nil
---@return number
function net.ping(peer) end
---Send a named remote call. On the server it goes to clients (all, or `to`);
---on a client it goes to the server. Args: scalars + tables (≤4 deep, ≤1KB).
---Handle with `function onRpc.name(args, sender) end`.
---`withInput = true` (client → server) stamps the call with the tick you were
---SEEING when you fired — the server can then judge it with `net.rewind`.
---@param name string
---@param args any|nil
---@param opts { to: integer, withInput: boolean }|nil
function net.rpc(name, args, opts) end
---SERVER ONLY, inside an `onRpc` handler for an rpc sent `{withInput = true}`:
---run `fn` against the world as `peer` PERCEIVED it — raycasts see every
---networked body where that player saw it (their interp-delayed view), and
---other scripts' `synced` vars read the values from that same tick. A parry
---that was up on the attacker's screen counts. Restores the present after
---`fn`; returns whatever `fn` returns. Rewind depth is clamped to ~250 ms.
---@param peer integer The rpc's sender.
---@param fn function
---@return any ...
function net.rewind(peer, fn) end
---Listen for session events: \"playerJoined\"|\"playerLeft\" (fn gets the peer id),
---\"connected\", \"disconnected\" (fn gets a reason string).
---@param event string
---@param fn function
function net.on(event, fn) end
---SERVER ONLY: spawn a scene asset's first node as a replicated runtime object.
---It appears on every client (and late joiners). Available next tick.
---@param path string Scene asset, project-relative (e.g. \"scenes/arrow.ron\").
---@param opts { x: number, y: number, z: number, owner: integer }|nil
function net.spawn(path, opts) end
---SERVER ONLY: despawn a replicated runtime object everywhere.
---@param node Node
function net.despawn(node) end

---Per-script synced variables: declare `replicated = { hp = 100 }` at the top
---level, then read/write `synced.hp`. The SERVER's writes replicate to every
---client; client writes warn (the server will overwrite them).
---@type table<string, any>
synced = {}

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
---The ACTIVE camera's world yaw (radians), captured with the input snapshot.
---THE way to do camera-relative movement in multiplayer: the aim rides the
---input command, so the server and prediction replay use exactly the angle
---the player saw. nil when the scene has no active camera.
---@return number|nil
function input.aimYaw() end
---The active camera's world pitch (radians), captured with the input snapshot.
---@return number|nil
function input.aimPitch() end
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

---Cast a ray against the world's colliders (terrain + meshes + primitives)
---AND every physics body (players, crates). Returns a hit table
---{x,y,z, nx,ny,nz, distance, node} or nil — `node` is the hit body's node
---handle (nil for static geometry), so `hit.node:getscript(\"combat\")` works.
---Your OWN node's body is excluded (a ray from your center never hits you);
---pass another node as `ignore` to skip its body too (e.g. an orbit camera
---ignoring the character it follows).
---@param ox number
---@param oy number
---@param oz number
---@param dx number
---@param dy number
---@param dz number
---@param max number
---@param ignore Node|nil A node whose body the ray passes through.
---@return { x: number, y: number, z: number, nx: number, ny: number, nz: number, distance: number, node: Node|nil }|nil
function raycast(ox, oy, oz, dx, dy, dz, max, ignore) end

---Immediate-mode debug drawing (play mode): shapes show for ONE frame in the
---viewport, Scene AND Game views. Call every frame you want a shape visible.
---Colors are optional 0-1 floats (default green).
---@class Gizmo
gizmo = {}
---A world-space debug line.
---@param x1 number
---@param y1 number
---@param z1 number
---@param x2 number
---@param y2 number
---@param z2 number
---@param r? number
---@param g? number
---@param b? number
function gizmo.line(x1, y1, z1, x2, y2, z2, r, g, b) end
---A debug ray: origin + direction. With len the direction is normalized and
---the ray is that long (mirrors raycast) — great for visualizing ground checks.
---@param ox number
---@param oy number
---@param oz number
---@param dx number
---@param dy number
---@param dz number
---@param len? number
---@param r? number
---@param g? number
---@param b? number
function gizmo.ray(ox, oy, oz, dx, dy, dz, len, r, g, b) end
---A wire debug sphere (three rings): trigger zones, blast radii, ranges.
---@param x number
---@param y number
---@param z number
---@param radius? number
---@param r? number
---@param g? number
---@param b? number
function gizmo.sphere(x, y, z, radius, r, g, b) end
---A small 3-axis cross marking a spot: hit points, waypoints, spawns.
---@param x number
---@param y number
---@param z number
---@param size? number
---@param r? number
---@param g? number
---@param b? number
function gizmo.point(x, y, z, size, r, g, b) end
";

/// `.luarc.json` pointing the Lua language server at the annotation library and
/// declaring the engine globals (so they aren't flagged undefined).
pub(crate) const LUARC_JSON: &str = "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"fixedUpdate\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"assets\", \"spawnEffect\", \"net\", \"synced\", \"replicated\", \"onRpc\"]\n}\n";

/// Byte-exact PREVIOUS engine-generated `.luarc.json` versions: a project file
/// matching one of these was never hand-edited, so it's safe to migrate to the
/// current `LUARC_JSON` (a customized file is always left alone).
const LUARC_JSON_OLD: &[&str] = &[
    "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"assets\", \"spawnEffect\"]\n}\n",
];

/// Write the Lua language-server support files into a project (annotations always
/// refreshed; `.luarc.json` only if absent OR still an unmodified engine-generated
/// version — a user's own config is preserved).
pub(crate) fn write_lua_support(project_root: &Path) {
    let lib = project_root.join(".floptle").join("library");
    let _ = std::fs::create_dir_all(&lib);
    let _ = std::fs::write(lib.join("floptle.lua"), LUA_ANNOTATIONS);
    let luarc = project_root.join(".luarc.json");
    let migrate = match std::fs::read_to_string(&luarc) {
        Ok(cur) => LUARC_JSON_OLD.contains(&cur.as_str()),
        Err(_) => true, // absent
    };
    if migrate {
        let _ = std::fs::write(luarc, LUARC_JSON);
    }
}

/// Write the default scripts into `scripts_dir` (each only if absent).
pub(crate) fn seed_default_scripts(scripts_dir: &Path) {
    let _ = std::fs::create_dir_all(scripts_dir);
    for (name, body) in DEFAULT_SCRIPTS {
        let p = scripts_dir.join(name);
        if !p.exists() {
            let _ = std::fs::write(&p, body);
        }
    }
}
