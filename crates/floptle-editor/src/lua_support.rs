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
---@field pos Vec3 The node's position as a vec3 (read/write: `node.pos = node.pos + dir * dt`). Accepts any {x=,y=,z=} value.
---@field layer string Collision/query layer, by project-defined NAME (\"Default\" when unset). Assigning a name the project doesn't define is an ERROR — add layers in Project Settings.
---@field tags string[] The node's tags (a fresh array each read). Assign a whole array to replace; use addTag/removeTag for single edits.
---@field hasTag fun(self: Node, tag: string): boolean Whether the node carries this exact tag.
---@field destroy fun(self: Node) Remove this node and its whole subtree (queued; applied after the pass). Same as `destroy(node)`.
---@field addTag fun(self: Node, tag: string) Add a tag (duplicates are ignored).
---@field removeTag fun(self: Node, tag: string) Remove a tag (no-op when absent).
---@field height number Physics (capsule bodies): standing height - write a smaller value to crouch.
---@field text string|nil UI elements: the label's text (write to change it — numbers coerce, so `hp.text = 42` works).
---@field getcomponent fun(self: Node, name: string): RigidBodyHandle|PointLightHandle|CameraHandle|UiElementHandle|UiSliderHandle|UiLayerHandle|nil Live component handle (RigidBody / PointLight / Camera / ParticleSystem / AudioSource / UiElement / UiSlider / UiLayer), nil if the node lacks it.
---@field particles fun(self: Node): ParticleSystemHandle The particle handle for this node's Particle System: play / stop / restart the effect and read its live state.
---@field sound fun(self: Node): AudioSourceHandle The sound handle for this node's Audio Source: play / stop / pause / swap clips and read playback state.

---A Rigidbody's live tunables (every Inspector field). Assign to change while playing;
---booleans may be written true/false and read back as 1/0.
---@class RigidBodyHandle
---@field friction number Surface friction 0..1 (0 = frictionless).
---@field restitution number Bounciness 0..1 (0 = no bounce).
---@field gravity number Gravity pull on this body (1/0; assign true/false).
---@field kinematic number Transform-driven mode (1/0; assign true/false, live): never falls or gets pushed, but PUSHES dynamic bodies — platforms, elevators, grabbed objects. (Static mode is the Inspector dropdown — a baked collider, nothing to toggle here.)
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

---A Camera's live properties (`node:getcomponent(\"Camera\")`).
---@class CameraHandle
---@field fovY number Vertical field of view, radians.
---@field active number The play-mode view camera (1/0) — assign true to switch to it.

---A UI element's live properties (`node:getcomponent(\"UiElement\")`) — drive a HUD
---from scripts. Position/size numbers follow whatever mode the Inspector set
---(px, %, grow); `text` content is `node.text`.
---@class UiElementHandle
---@field visible number Shown (1/0; assign true/false).
---@field opacity number Multiplies every color the element draws, 0..1.
---@field posX number Free position X / Pin offset X (design units).
---@field posY number Free position Y / Pin offset Y (design units).
---@field width number Width in the axis's sizing mode (px value, % fraction, or grow weight). Absent (nil) on a fit axis; writing one makes it fixed px.
---@field height number Height (same rules as width).
---@field radius number Shape corner radius (design units).
---@field border number Shape border thickness (design units).
---@field fillR number Shape fill red 0..1.
---@field fillG number Shape fill green 0..1.
---@field fillB number Shape fill blue 0..1.
---@field fillA number Shape fill alpha 0..1.
---@field textSize number Text glyph size (design units; ignored while fit is on).
---@field textR number Text color red 0..1.
---@field textG number Text color green 0..1.
---@field textB number Text color blue 0..1.
---@field textA number Text color alpha 0..1.
---@field tintR number Image tint red 0..1.
---@field tintG number Image tint green 0..1.
---@field tintB number Image tint blue 0..1.
---@field tintA number Image tint alpha 0..1.
---@field cell number Spritesheet cell index the image shows (set per frame for sprite animation).

---A UI slider's live value (`node:getcomponent(\"UiSlider\")`) — the health-bar hook:
---`bar:getcomponent(\"UiSlider\").value = hp` and the Fill/Handle parts follow.
---@class UiSliderHandle
---@field value number Current value (clamped to min..max at draw time).
---@field min number Range start.
---@field max number Range end.

---A UI layer's live properties (`node:getcomponent(\"UiLayer\")`).
---@class UiLayerHandle
---@field enabled number Master switch (1/0; assign true/false) — an off layer draws nothing.
---@field z number Draw order: lowest z first.
---@field designHeight number Design units that span the window height.
---@field worldSpace number 1 = a panel inside the 3D world at this node's transform; 0 = a screen overlay.

---A node's Particle System, controlled from a script via `node:particles()`.
---Start/stop the effect at runtime and read whether it's playing.
---@class ParticleSystemHandle
---@field play fun(self: ParticleSystemHandle) Start emitting if idle (spawns a fresh instance).
---@field stop fun(self: ParticleSystemHandle) Stop + despawn — the live particles vanish.
---@field restart fun(self: ParticleSystemHandle) Re-spawn from t=0 (re-fire a one-shot burst).
---@field isPlaying fun(self: ParticleSystemHandle): boolean Is an instance emitting/ageing right now?
---@field alive fun(self: ParticleSystemHandle): number Live particle count across the effect's tracks.
---@field asset fun(self: ParticleSystemHandle): string|nil The effect asset key this node references.

---A node's Audio Source, controlled from a script via `node:sound()`.
---@class AudioSourceHandle
---@field play fun(self: AudioSourceHandle) Play the source's clip from the start (restarts if already playing).
---@field stop fun(self: AudioSourceHandle) Fade the sound out (a few ms — no click).
---@field pause fun(self: AudioSourceHandle) Freeze playback (resume continues from here).
---@field resume fun(self: AudioSourceHandle) Continue a paused sound.
---@field setClip fun(self: AudioSourceHandle, clip: string) Swap the clip (project-relative path like \"audio/steps.ogg\"); restarts playback if playing.
---@field seek fun(self: AudioSourceHandle, secs: number) Jump the playhead to a time in seconds.
---@field isPlaying fun(self: AudioSourceHandle): boolean Is the source audible right now?
---@field position fun(self: AudioSourceHandle): number Playhead in seconds.

---A playing sound returned by `audio.play(...)`. Handles stay valid until the
---sound finishes; calls on a finished sound are ignored.
---@class SoundHandle
---@field stop fun(self: SoundHandle) Fade the sound out and end it.
---@field pause fun(self: SoundHandle) Freeze playback.
---@field resume fun(self: SoundHandle) Continue a paused sound.
---@field setVolume fun(self: SoundHandle, volume: number) Linear volume (1 = as authored).
---@field setPitch fun(self: SoundHandle, pitch: number) Playback-rate pitch (0.5 = octave down, 2 = octave up).
---@field setPan fun(self: SoundHandle, pan: number) Stereo pan −1..1 (non-spatial sounds).
---@field setTrack fun(self: SoundHandle, track: string) Re-route through a mixer track (\"Master\" or a track name).
---@field setPosition fun(self: SoundHandle, x: number, y: number, z: number) Move the emitter (stops following a node).
---@field seek fun(self: SoundHandle, secs: number) Jump the playhead to a time in seconds.
---@field isPlaying fun(self: SoundHandle): boolean Still audible (false once finished)?
---@field position fun(self: SoundHandle): number Playhead in seconds.

---A mixer track handle from `audio.track(name)` — live control of the
---project mixer (reverts to the saved mixer when Play stops).
---@class AudioTrackHandle
---@field setVolume fun(self: AudioTrackHandle, db: number) Fader gain in dB (0 = unity, −60 = silent).
---@field setPan fun(self: AudioTrackHandle, pan: number) Stereo pan −1..1.
---@field setMuted fun(self: AudioTrackHandle, muted: boolean) Mute / unmute the track.
---@field setSoloed fun(self: AudioTrackHandle, soloed: boolean) Solo the track (mutes everything else).

---Options for `audio.play`. All fields optional.
---@class AudioPlayOpts
---@field volume number? Linear volume (default 1).
---@field pitch number? Playback rate (default 1; also shifts pitch).
---@field pan number? Stereo pan −1..1 (non-spatial sounds).
---@field mode string? \"Spatial\" (default) | \"Distance\" (no panning) | \"Flat\" (2D).
---@field falloff string? \"Inverse\" (default) | \"Linear\" | \"Exponential\".
---@field minDistance number? Full volume inside this range (default 2).
---@field maxDistance number? Silent past this range (default 50).
---@field track string? Mixer track to route through (default Master).
---@field endBehavior string? \"Stop\" (default) | \"Destroy\" (despawn the followed node) | \"Loop\".
---@field loop boolean? Shorthand for endBehavior = \"Loop\".

---The sound system: fire-and-forget playback + mixer control. Positions and
---following make it spatial; pass no position for flat 2D (UI, music).
---@class audio
---@field play fun(clip: string, a?: Node|number, b?: number|AudioPlayOpts, c?: number, opts?: AudioPlayOpts): SoundHandle Play a clip: `audio.play(\"audio/ding.ogg\")` (flat) · `audio.play(\"audio/hit.ogg\", x, y, z, opts)` (at a point) · `audio.play(\"audio/engine.ogg\", carNode, {loop=true})` (follows the node).
---@field stopAll fun() Stop every playing sound (sources and one-shots).
---@field track fun(name: string): AudioTrackHandle A mixer track handle (\"Master\" or a track name from the Mixer tab).
audio = {}

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

---Spawn a PREFAB instance (make one by dragging a node into the Assets panel).
---`\"bullet\"` finds `prefabs/bullet.prefab.ron`; subfolder names and full
---paths work too. `pos` places the first root (a vec3/table/node); `fn(root)`
---runs with the new node's handle the same frame:
---`spawn(\"bullet\", node.pos + dir, function(b) b.vx = dir.x * 40 end)`
---Local-only in multiplayer — the server uses `net.spawn` for replicated objects.
---@param prefab string Prefab name or path.
---@param pos? Vec3 World position for the first root.
---@param fn? fun(root: Node) Configure the freshly spawned root.
function spawn(prefab, pos, fn) end

---Remove a node AND its whole subtree (physics body included). Queued —
---applied after the pass, so the handle stays readable through this call.
---Method form: `node:destroy()`. On a client, replicated nodes refuse
---(server authority — use `net.despawn` on the server).
---@param target Node The node (or node handle) to remove.
function destroy(target) end

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

---Runs once per frame AFTER physics and the interpolated transform writeback —
---the CAMERA pass. Anything that follows something else (orbit cameras, name
---tags, listeners) belongs here so it samples this frame's FINAL poses;
---following from `update` reads last frame's pose (a velocity × dt lag that
---turns frame-time noise into visible jitter).
---@param node Node
---@param dt number Seconds since the last frame.
function lateUpdate(node, dt) end

---Mark a `defaults` entry as a NODE REFERENCE: `defaults = { hpBar = noderef() }`
---shows a node picker in the Inspector (or drag a node from the Hierarchy onto
---the slot), and the script reads `params.hpBar` as a node handle (or nil while
---unwired) — no `find()` needed.
---@return any
function noderef() end

---Mark a `defaults` entry as a SCRIPT REFERENCE: `defaults = { hp = scriptref(\"health\") }`
---binds to that script ON the wired node — `params.hp` is a script handle directly
---(call its functions, read its state). The Inspector lists only nodes carrying it.
---@param kind string The script name (its .lua file stem).
---@return any
function scriptref(kind) end

---Mark a `defaults` entry as a COMPONENT REFERENCE: `defaults = { body = componentref(\"RigidBody\") }`
---binds to that component ON the wired node — `params.body.friction = 0.05` directly.
---Components: RigidBody, PointLight, Camera, ParticleSystem, UiElement, UiSlider, UiLayer.
---@param name string The component name.
---@return any
function componentref(name) end

---UI button hook: fires when this node's UI element (with `button` on) is clicked
---(pressed AND released on it). Also available: `pressed`, `released`,
---`hoverStart`, `hoverEnd` — same signature. Style the states here (no imposed look).
---@param node Node
function clicked(node) end

---UI button hook: the pointer entered this node's element. Pair with `hoverEnd`.
---@param node Node
function hoverStart(node) end

---Multiplayer (docs/netcode-design.md). Mark nodes with the Networked component,
---declare synced vars with a top-level `replicated = { hp = 100 }` table (read/
---write them as `synced.hp` — the server owns them), handle remote calls with
---`onRpc = {}` + `function onRpc.name(args, sender) end`.
---@class Net
net = {}
---Become the authoritative host. `relay = \"addr\"` hosts through a
---rendezvous relay: you get a LOBBY CODE, friends join with it, nobody
---port-forwards. `port = n` hosts directly on UDP (QUIC) for LAN/self-host.
---Neither: the in-editor loopback harness.
---@param opts { maxPlayers: integer, port: integer, relay: string }|nil
function net.host(opts) end
---Join a session: `\"relay://relayaddr/CODE\"` = a lobby code through a
---relay (no port-forwarding), `\"quic://host:port\"` = a server directly,
---`\"local://\"` = the in-editor test harness.
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
---Is this node under MY control on this machine? Offline / non-networked →
---true. Server → true unless a remote peer owns it. Client → true only for
---your own predicted node(s). THE way for shared scripts (cameras, HUDs) to
---pick the local player out of many identical avatars:
---`for _, s in ipairs(findScripts(\"third_person\")) do if net.isMine(s.node) then ... end end`
---@param node Node
---@return boolean
function net.isMine(node) end

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
---ignoring the character it follows). The last arg can instead be an OPTIONS
---table: `raycast(x,y,z, dx,dy,dz, max, { ignore = target, layers = {\"Ground\"} })`
---— `layers` (a name or an array of names, Project Settings → Layers) filters
---BOTH static geometry and bodies; a misspelled layer name is an error.
---@param ox number
---@param oy number
---@param oz number
---@param dx number
---@param dy number
---@param dz number
---@param max number
---@param ignore Node|{ ignore: Node|nil, layers: string|string[]|nil }|nil A node whose body the ray passes through, or an options table.
---@return { x: number, y: number, z: number, nx: number, ny: number, nz: number, distance: number, node: Node|nil }|nil
function raycast(ox, oy, oz, dx, dy, dz, max, ignore) end

---EVERY node carrying script `kind`, as script handles in scene order — for
---picking among several instances (a camera finding the one third_person
---that is `net.isMine`, out of many player avatars).
---@param kind string
---@return table[]
function findScripts(kind) end

---EVERY node carrying tag `tag` (Inspector \"tags\" chips / node:addTag), as
---node handles in scene order — an empty table when none.
---`findTagged(\"enemy\")[1]` grabs the first.
---@param tag string
---@return Node[]
function findTagged(tag) end

---A 3-component vector value with real operators: `a + b`, `a - b`, `v * 2`,
---`v / 2`, `-v`, `a == b`. Anything that ACCEPTS a vector also accepts a plain
---{x=, y=, z=} table or a node handle.
---@class Vec3
---@field x number
---@field y number
---@field z number
---@field length fun(self: Vec3): number
---@field lengthSquared fun(self: Vec3): number
---@field normalized fun(self: Vec3): Vec3 Unit-length copy (zero stays zero).
---@field dot fun(self: Vec3, other: Vec3): number
---@field cross fun(self: Vec3, other: Vec3): Vec3
---@field lerp fun(self: Vec3, other: Vec3, t: number): Vec3
---@field distance fun(self: Vec3, other: Vec3): number

---A 2-component vector (UI/screen math) — same operators as Vec3.
---@class Vec2
---@field x number
---@field y number
---@field length fun(self: Vec2): number
---@field lengthSquared fun(self: Vec2): number
---@field normalized fun(self: Vec2): Vec2
---@field dot fun(self: Vec2, other: Vec2): number
---@field lerp fun(self: Vec2, other: Vec2, t: number): Vec2
---@field distance fun(self: Vec2, other: Vec2): number

---Make a vec3: `vec3()` = zero, `vec3(s)` = splat, `vec3(x, y, z)`, or
---`vec3(other)` = copy (also from a {x=,y=,z=} table or node).
---@param x number|Vec3|Node|nil
---@param y number|nil
---@param z number|nil
---@return Vec3
function vec3(x, y, z) end

---Make a vec2: `vec2()` = zero, `vec2(s)` = splat, `vec2(x, y)`.
---@param x number|Vec2|nil
---@param y number|nil
---@return Vec2
function vec2(x, y) end

---Distance between two points: vectors, {x=,y=,z=} tables, or NODE handles —
---`distance(node, target)` just works. Also `distance(x1,y1,z1, x2,y2,z2)`.
---@param a Vec3|Vec2|Node|number
---@param b Vec3|Vec2|Node|number|nil
---@return number
function distance(a, b, ...) end

---The contact info passed to collision/trigger hooks: world point + normal.
---@class Hit
---@field x number Contact point X (world).
---@field y number Contact point Y (world).
---@field z number Contact point Z (world).
---@field nx number Contact normal X (unit, out of the hit surface).
---@field ny number Contact normal Y.
---@field nz number Contact normal Z.

---Fires the tick two nodes START touching (this node's body vs a solid
---collider, or vs another body). `other` is the other node's handle.
---@param node Node
---@param other Node
---@param hit Hit
function onCollisionEnter(node, other, hit) end

---Fires every tick while the touch lasts (resting on the ground reports its
---floor node every tick — gate on `other:hasTag(...)` etc.).
---@param node Node
---@param other Node
---@param hit Hit
function onCollisionStay(node, other, hit) end

---Fires the tick the pair separates (hit = the last known contact).
---@param node Node
---@param other Node
---@param hit Hit
function onCollisionExit(node, other, hit) end

---Fires the tick a body ENTERS a trigger (a Collider with the \"trigger\"
---switch on: no blocking, events only — portals, pickup zones, checkpoints).
---@param node Node
---@param other Node
---@param hit Hit
function onTriggerEnter(node, other, hit) end

---Fires every tick a body stays inside the trigger.
---@param node Node
---@param other Node
---@param hit Hit
function onTriggerStay(node, other, hit) end

---Fires the tick a body LEAVES the trigger.
---@param node Node
---@param other Node
---@param hit Hit
function onTriggerExit(node, other, hit) end

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

---Scene management: the running scene and transitions between scenes.
---In multiplayer only the SERVER may switch — every client follows
---automatically (a joined client's `scene.load` is refused; send the server
---an RPC and let its script decide).
---@class Scene
scene = {}
---Queue a transition to another scene, performed at the next frame boundary:
---the world swaps to the new scene, physics/animators/particles/audio rebuild
---against it, and every script's `start` re-fires — exactly like the scene
---booting fresh. Accepts a name (\"arena\"), a scenes-relative path
---(\"arenas/desert\"), or a project-relative path (\"scenes/arena.ron\").
---@param name string
function scene.load(name) end
---The running scene's name (its file stem, e.g. \"first\").
---@return string
function scene.current() end
---Every scene in the project, as names `scene.load` accepts (sorted;
---subfolders kept, e.g. \"arenas/desert\").
---@return string[]
function scene.list() end

---Runtime terrain editing + queries (Terrain 2.0). Edits queue and land the
---same tick (collision updates with the surface). World coordinates.
---In multiplayer, run edits on the SERVER and mirror them with an RPC that
---repeats the same call — the ops are deterministic.
---@class Terrain
terrain = {}
---Sculpt the nearest terrain at (x,y,z): mode \"raise\" (default), \"lower\"/\"dig\",
---\"smooth\", or \"flatten\". strength 0..1 (default 1). No-op when no terrain
---surface is near the point.
---@param x number
---@param y number
---@param z number
---@param radius number
---@param strength? number
---@param mode? string
function terrain.sculpt(x, y, z, radius, strength, mode) end
---Dig a hole — sugar for `terrain.sculpt(x, y, z, radius, strength, \"lower\")`.
---@param x number
---@param y number
---@param z number
---@param radius number
---@param strength? number
function terrain.dig(x, y, z, radius, strength) end
---Recolor the terrain surface inside the brush ball (r/g/b are 0..1).
---@param x number
---@param y number
---@param z number
---@param radius number
---@param r number
---@param g number
---@param b number
---@param strength? number
function terrain.paint(x, y, z, radius, r, g, b, strength) end
---Paint a terrain-palette texture slot (1-based, the Terrain tab's palette;
---0 clears back to the flat color).
---@param x number
---@param y number
---@param z number
---@param radius number
---@param slot number
function terrain.paintTexture(x, y, z, radius, slot) end
---Signed distance from (x,y,z) to the nearest terrain surface (negative =
---inside rock), or nil when the scene has no terrain.
---@param x number
---@param y number
---@param z number
---@return number|nil
function terrain.query(x, y, z) end
---World Y of the highest terrain surface under (x,z), or nil when no terrain
---is hit there.
---@param x number
---@param z number
---@return number|nil
function terrain.height(x, z) end

---Seeded value noise, one octave, ≈ -1..1. Deterministic on every machine —
---the SAME numbers the engine's Rust generators produce. Scale the inputs to
---pick a frequency (lattice cell = 1 unit).
---@param x number
---@param y number
---@param z number
---@param seed? number
---@return number
function math.noise(x, y, z, seed) end

---Seeded fractal noise (fbm): `octaves` layers (default 4), rotated so features
---never align to the axes. ≈ -1..1, deterministic everywhere.
---@param x number
---@param y number
---@param z number
---@param octaves? number
---@param seed? number
---@return number
function math.fbm(x, y, z, octaves, seed) end

---A deterministic random stream: the same seed gives the same sequence on
---every machine. Use for gameplay that must reproduce (loot, procgen scatter,
---anything a server might replay); `math.random` stays for throwaway rolls.
---@class Rng
---@field next fun(self: Rng): number Uniform in [0, 1).
---@field range fun(self: Rng, a: number, b: number): number Uniform in [a, b).
---@field int fun(self: Rng, a: number, b: number): integer Uniform integer in [a, b] inclusive.
---@field pick fun(self: Rng, list: any[]): any A uniform element of `list` (nil if empty).

---Make a deterministic random stream from a seed.
---@param seed number
---@return Rng
function rng(seed) end

---Persistent game data: a per-slot key→value store that survives Play
---sessions, editor restarts, and ships with exported builds. Values take the
---synced-var guardrails (numbers/strings/bools/tables ≤ depth 4, ≤ 1 KB).
---Flushes on Stop + every few seconds during Play; `save.flush()` forces it.
---Multiplayer: LOCAL storage — for server-authoritative progress call save.*
---on the server and hand results to clients via synced/RPC.
---@class Save
save = {}
---Store a value under `key` (guardrails apply; violations are script errors).
---@param key string
---@param value any
function save.set(key, value) end
---The stored value, else `default`, else nil.
---@param key string
---@param default? any
---@return any
function save.get(key, default) end
---Remove a key. Returns true if something was removed.
---@param key string
---@return boolean
function save.delete(key) end
---Switch the active save slot (flushing the old one), or read the current
---slot's name when called with no argument. Names: letters/digits/-/_ only.
---@param name? string
---@return string
function save.slot(name) end
---Write the store to disk now (checkpoints). Returns false on an IO error
---(also surfaced in the Console).
---@return boolean
function save.flush() end

---A scheduled timer. `cancel()` aborts it (safe to call after it fired).
---@class TimerHandle
---@field cancel fun(self: TimerHandle)

---Run `fn` once after `seconds` of GAME TIME (tick-driven and deterministic;
---paused when the game is paused). The callback gets no arguments — capture
---what you need as locals. Errors log to the Console and kill only that timer.
---@param seconds number
---@param fn fun()
---@return TimerHandle
function after(seconds, fn) end

---Run `fn` repeatedly, first after `seconds`, then every `seconds` (anchored:
---long sessions don't drift). Cancel via the returned handle.
---@param seconds number
---@param fn fun()
---@return TimerHandle
function every(seconds, fn) end

---Animate: call `fn(alpha)` every tick for `seconds`, alpha easing 0→1, final
---call guaranteed exactly at 1.0. `ease` is \"linear\" (default), \"smooth\",
---\"in\", or \"out\". e.g. `tween(0.5, function(a) node.y = a * 3 end, \"smooth\")`
---@param seconds number
---@param fn fun(alpha: number)
---@param ease? \"linear\"|\"smooth\"|\"in\"|\"out\"
---@return TimerHandle
function tween(seconds, fn, ease) end
";

/// `.luarc.json` pointing the Lua language server at the annotation library and
/// declaring the engine globals (so they aren't flagged undefined).
pub(crate) const LUARC_JSON: &str = "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"fixedUpdate\", \"lateUpdate\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findScripts\", \"findTagged\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"destroy\", \"spawnEffect\", \"scene\", \"terrain\", \"rng\", \"save\", \"after\", \"every\", \"tween\", \"audio\", \"net\", \"synced\", \"replicated\", \"onRpc\"]\n}\n";

/// Byte-exact PREVIOUS engine-generated `.luarc.json` versions: a project file
/// matching one of these was never hand-edited, so it's safe to migrate to the
/// current `LUARC_JSON` (a customized file is always left alone).
const LUARC_JSON_OLD: &[&str] = &[
    "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"fixedUpdate\", \"lateUpdate\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findScripts\", \"findTagged\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"destroy\", \"spawnEffect\", \"scene\", \"terrain\", \"rng\", \"save\", \"audio\", \"net\", \"synced\", \"replicated\", \"onRpc\"]\n}\n",
    "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"fixedUpdate\", \"lateUpdate\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findScripts\", \"findTagged\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"destroy\", \"spawnEffect\", \"scene\", \"terrain\", \"rng\", \"audio\", \"net\", \"synced\", \"replicated\", \"onRpc\"]\n}\n",
    "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"fixedUpdate\", \"lateUpdate\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findScripts\", \"findTagged\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"destroy\", \"spawnEffect\", \"scene\", \"terrain\", \"audio\", \"net\", \"synced\", \"replicated\", \"onRpc\"]\n}\n",
    "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"fixedUpdate\", \"lateUpdate\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findScripts\", \"findTagged\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"destroy\", \"spawnEffect\", \"scene\", \"audio\", \"net\", \"synced\", \"replicated\", \"onRpc\"]\n}\n",
    "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"fixedUpdate\", \"lateUpdate\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findScripts\", \"findTagged\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"destroy\", \"spawnEffect\", \"scene\", \"net\", \"synced\", \"replicated\", \"onRpc\"]\n}\n",
    "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findTagged\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"destroy\", \"spawnEffect\", \"scene\"]\n}\n",
    "{\n  \"runtime.version\": \"Lua 5.1\",\n  \"workspace.library\": [\".floptle/library\"],\n  \"diagnostics.globals\": [\"node\", \"params\", \"time\", \"dt\", \"defaults\", \"start\", \"update\", \"fixedUpdate\", \"lateUpdate\", \"log\", \"input\", \"raycast\", \"gizmo\", \"find\", \"findAll\", \"findScript\", \"findScriptInScene\", \"findTagged\", \"vec2\", \"vec3\", \"distance\", \"onCollisionEnter\", \"onCollisionStay\", \"onCollisionExit\", \"onTriggerEnter\", \"onTriggerStay\", \"onTriggerExit\", \"assets\", \"spawn\", \"destroy\", \"spawnEffect\", \"scene\", \"net\", \"synced\", \"replicated\", \"onRpc\"]\n}\n",
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
