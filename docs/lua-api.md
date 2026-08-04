# Lua API reference

Every name a script can reach, grouped the way the editor's **Docs** tab groups
them. The same table drives this page, that tab, the hover docs and autocomplete —
so there is one description of each call, in one place, and it is the one you get
everywhere.

*Generated — do not edit by hand.* Change the entry in `crates/floptle-editor/src/ide.rs`
and run `UPDATE_DOCS=1 cargo test -p floptle-editor lua_api_reference_file`.

New here? [`scripting.md`](scripting.md) is the guided tour — it teaches in order,
with worked examples. This page is the reference: complete, alphabetical within
each group, and meant to be searched.

## Contents

- [script basics — lifecycle, params, log](#script-basics--lifecycle-params-log) — 22
- [node — transform & body fields](#node--transform--body-fields) — 36
- [node — methods & handles](#node--methods--handles) — 17
- [vectors, directions & easing](#vectors-directions--easing) — 47
- [scene lookups & raycast](#scene-lookups--raycast) — 16
- [references — wire nodes in the Inspector](#references--wire-nodes-in-the-inspector) — 3
- [input — keyboard & mouse](#input--keyboard--mouse) — 42
- [drawing — draw.*](#drawing--draw) — 13
- [the web — http.*, json.*](#the-web--http-json) — 9
- [the player's account — account.*](#the-players-account--account) — 13
- [game UI — text, buttons & hooks](#game-ui--text-buttons--hooks) — 70
- [networking — net.*, synced](#networking--net-synced) — 31
- [scenes — load, unload & persist](#scenes--load-unload--persist) — 6
- [terrain — runtime sculpt & queries](#terrain--runtime-sculpt--queries) — 14
- [water — depth, buoyancy & ice](#water--depth-buoyancy--ice) — 6
- [scatter — instanced props](#scatter--instanced-props) — 8
- [2D — tilemaps & sprite batches](#2d--tilemaps--sprite-batches) — 9
- [vessels — assembly.*](#vessels--assembly) — 14
- [the camera & the screen](#the-camera--the-screen) — 7
- [physics controls — pause & step](#physics-controls--pause--step) — 4
- [persistence — save.*](#persistence--save) — 7
- [timers — after, every, tween](#timers--after-every-tween) — 4
- [space — orbits & time-warp](#space--orbits--time-warp) — 19
- [components — getcomponent](#components--getcomponent) — 27
- [animation — node:animator](#animation--nodeanimator) — 16
- [particles — effects from script](#particles--effects-from-script) — 10
- [audio — sounds & the mixer](#audio--sounds--the-mixer) — 27
- [assets](#assets) — 3
- [debug gizmos](#debug-gizmos) — 5
- [lua stdlib](#lua-stdlib) — 43

## script basics — lifecycle, params, log

### `createNode`

createNode(name [, parent] [, fn]) — create a PLAIN node (Empty matter). fn(n) gets its handle: combine with n:setTerrain(id) / n:setCelestial{...} / n:setPrimitive(shape, color) / n:setMaterial{...} + transform writes to build content from script (procgen, editor actions). Nested creates inside callbacks are fine.

### `defaults`

defaults = { name = value } — tunables shown in the Inspector.

```lua
defaults = {
  --@header Movement
  -- How fast you walk on flat ground.
  --@range 0 20 --@units m/s
  walk = 4.5,
  --@options Off|On|Auto
  assist = 1,
  invert = false,
}
```

### `destroy`

destroy(node) — remove a node AND its whole subtree (physics body included). Queued: applied after the pass, so the handle stays readable through the current call. Method form: node:destroy(). On a client, replicated nodes refuse (server authority — net.despawn).

### `dt`

Seconds since the last frame (number).

### `fixedUpdate`

function fixedUpdate(node, dt) — runs every GAMEPLAY TICK (60 Hz, constant dt). Movement/gameplay/physics writes belong here; cameras & followers in lateUpdate; other cosmetics in update. Same cadence physics steps at — frame-rate independent.

```lua
-- gameplay writes belong on the tick, not the frame
function fixedUpdate(node, dt)
  node.vel = node.vel + vec3(0, -9.8, 0) * dt
end
```

### `function`

Define a function.

### `http`

Talk to a web server: http.get / post / put / delete, plus json.*. Every call is asynchronous — the reply arrives in a callback, never as a return value.

### `json`

json.encode(t) and json.decode(s) — the wire format for http.*. decode returns nil, message on bad input rather than raising, because a reply from someone else's server is data, not a bug in your script.

### `lateUpdate`

function lateUpdate(node, dt) — runs once per frame AFTER physics and the interpolated transform writeback: the CAMERA pass. Anything that follows something else (orbit cameras, name tags, listeners) belongs here so it samples this frame's FINAL poses. Following from update reads LAST frame's pose — a velocity × dt lag that turns frame-time noise into visible jitter.

```lua
-- follow AFTER physics, so the camera samples this frame's final pose
function lateUpdate(node, dt)
  local t = find("Player")
  node.pos = t.pos + t.forward * -6 + vec3(0, 2, 0)
end
```

### `local`

Declare a local variable.

### `log`

log("message") — print to the engine console.

### `node`

The node's transform: x/y/z, scale, scale_x/y/z, yaw/pitch/roll.

### `onCollisionEnter`

function onCollisionEnter(node, other, hit) — fires the tick this node's body STARTS touching something solid (a collider or another body). `other` = the other node's handle (check other:hasTag("...") / other.name); hit = { x, y, z, nx, ny, nz } (world contact point + normal). Also onCollisionStay (every tick while touching) and onCollisionExit (on separation).

```lua
function onCollisionEnter(node, other)
  if other:hasTag("hazard") then hp = hp - 10 end
end
```

### `onCollisionExit`

function onCollisionExit(node, other, hit) — fires the tick the touch ends (hit = the last known contact).

### `onTriggerEnter`

function onTriggerEnter(node, other, hit) — fires the tick a body enters a TRIGGER (the "trigger" switch on a Collider or Rigidbody: it stops blocking, events still fire — a Kinematic trigger rigidbody = a moving pickup). The portal/pickup/checkpoint hook — pair with a string param: scene.load(params.destination). Also onTriggerStay / onTriggerExit.

### `onTriggerExit`

function onTriggerExit(node, other, hit) — fires the tick a body leaves the trigger.

### `params`

This instance's tunables, a table seeded from `defaults` (params.speed, …). NUMBERS and STRINGS both work — a string default (destination = "arena") becomes an Inspector text field, so two portals can share one script with different destinations. TWO-WAY: writing a declared key persists across frames, shows live in the Inspector during Play, and is readable by other scripts through a handle (Stop reverts it). Undeclared keys stay frame-local; reference params (noderef & friends) never round-trip.

```lua
function update(node, dt)
  node.x = node.x + params.walk * dt   -- Inspector-tuned
end
```

### `spawn`

spawn(prefab [, pos [, fn]]) — spawn a PREFAB instance (make one by dragging a node into the Assets panel). "bullet" finds prefabs/bullet.prefab.ron. pos = a vec3/node for the root; fn(root) runs with the new node's handle the same frame — spawn("bullet", node.pos + dir, function(b) b.vx = dir.x * 40 end). Local-only in multiplayer: the server uses net.spawn for replicated objects.

```lua
local b = spawn("Bullet", node.pos + node.forward * 1.5, function(n)
  n.vel = node.forward * 40
end)
```

### `start`

function start(node) — runs once when play begins.

### `time`

Seconds since play started (number).

### `ui`

Screen UI from scripts: ui.on / ui.events for input, ui.bind for data, ui.make for whole trees. See the game-UI section for the full set.

### `update`

function update(node, dt) — runs every frame while playing.

```lua
function update(node, dt)
  node.yaw = node.yaw + math.rad(90) * dt
end
```

## node — transform & body fields

### `node.forward`

The node's facing as a vec3, from its rotation (-Z forward, matching the camera). Works on anything with a transform, body or not.

```lua
-- facing, from the node's rotation: -Z forward, +X right
local aim = node.forward
if raycast(node.pos, aim, 50) then log("something ahead") end
```

### `node.groundNormal`

The floor the body is standing on, as a vec3 normal — nil when airborne, so it is exactly node.grounded with the surface attached. Read-only. `node.groundNormal:dot(node.up)` is the cosine of the slope: 1 is flat, 0.5 is 60°. Align a character to the ground, judge a landing, or refuse to walk up something too steep.

### `node.grounded`

True while the rigidbody rests on a surface (read-only). Gate jumps on it.

### `node.height`

Capsule standing height — write a smaller value to crouch (the engine resizes it, feet planted).

### `node.id`

A stable numeric id for this node.

### `node.layer`

The node's collision/query layer, by project-defined NAME ("Default" when unset). Assign to move it (node.layer = "Ghosts") — a name the project doesn't define is an ERROR, so typos surface immediately. The Project Settings matrix decides which layers collide; a dynamic body re-layers live.

### `node.material`

Apply a material — assign a preset name ("Gold") or an assets.getFile("materials/X.ron").

### `node.model`

A Mesh node's model path — read it, or ASSIGN it to swap the model live (e.g. node.model = assets.getFile("models/x.glb")).

### `node.name`

The node's name (string).

### `node.parent`

The parent node handle, or nil. A handle has the same fields (x/y/z, …) so you can read/write another node.

### `node.pitch`

Pitch about X, in radians.

### `node.pos`

The node's position as a vec3 (read/write): node.pos = node.pos + dir * dt. Accepts anything with x/y/z.

```lua
node.pos = node.pos + node.forward * (params.walk * dt)
```

### `node.right`

The node's +X axis as a vec3 (its rotation applied). Pairs with node.forward for camera-relative movement.

### `node.roll`

Roll about Z, in radians.

### `node.scale`

Uniform scale (shortcut). Setting it scales all axes.

### `node.scale_x`

Scale along X.

### `node.scale_y`

Scale along Y.

### `node.scale_z`

Scale along Z.

### `node.size`

The node's whole scale as a vec3 (read/write). `node.scale` stays the uniform-scale shortcut, and also accepts a vec3 when you want all three axes at once.

### `node.tags`

The node's tags as an array of strings (a fresh table each read). Assign a whole array to replace the list; use node:addTag / node:removeTag for single edits and node:hasTag to test.

### `node.tickPos`

The body's TICK pose as a vec3 (read/write) — where the simulation says it is, as opposed to node.pos, which is the interpolated pose the camera renders. Inside fixedUpdate use this one: move with node.tickPos = node.tickPos + vec3(d, 0, 0) and build hurtboxes from it. `node.x = node.x + d` in fixedUpdate teleports the body onto its VISUAL position, so the model slides and the hitbox doesn't follow. In a rollback match this is the difference between a hit registering and not.

### `node.tickYaw`

The body's tick-domain yaw (read/write) — node.yaw's simulation-truth counterpart, for facing a fighter inside fixedUpdate.

### `node.up`

The body's up as a vec3 — minus gravity, so Y on flat ground and RADIAL on a planet. The direction to jump in, wherever the player is standing.

```lua
-- the body's up (-gravity): Y on flat ground, radial on a planet
local lean = node.up:dot(vec3(0, 1, 0))
```

### `node.up_x`

Body up (−gravity) X — radial on a planet, so move along it for planet gravity. Read-only.

### `node.up_y`

Body up (−gravity) Y (read-only).

### `node.up_z`

Body up (−gravity) Z (read-only).

### `node.vel`

The body's velocity as a vec3 (read/write). `node.vel = node.vel + node.up * jump` replaces three vx/vy/vz lines, and it accepts anything with x/y/z.

```lua
-- one write instead of vx/vy/vz, and it reads as physics
if node.grounded and input.pressed("space") then
  node.vel = node.vel + node.up * params.jump
end
```

### `node.visible`

Whether the node's geometry is drawn — set node.visible = false to hide it (true to show).

### `node.vx`

Rigidbody velocity X (m/s). Read + write to drive the body; the engine integrates it.

### `node.vy`

Rigidbody velocity Y (m/s). Keep this for gravity/jump while replacing the horizontal part.

### `node.vz`

Rigidbody velocity Z (m/s).

### `node.wallNormal`

The steepest surface the body is pressed against, as a vec3 normal — the cliff you ran at, the crate you're shoving — or nil when there's nothing but floor. Read-only. This is what stops a controller launching itself: driving into a steep face means the solver pushes the capsule out along a normal that points partly UP, every frame, which reads as being fired into the sky. Take that component out of your movement (see first_person.lua's `slide`) and you slide along the face instead. Also: wall jumps, wall slides, 'you can't go that way'.

### `node.x`

World X position (number).

### `node.y`

World Y position (number).

### `node.yaw`

Heading about Y, in radians.

### `node.z`

World Z position (number).

## node — methods & handles

### `node:addTag`

node:addTag("burning") — add a tag at runtime (duplicates are ignored). findTagged sees it next frame.

### `node:animator`

node:animator() — the animation handle for this node's Animation Controller (or a rigged model's embedded clips). Setters: :play/:restart/:crossfade/:stop/:setSpeed/:setLayerWeight/:seek. Getters: :state/:time/:finished/:isPlaying/:clips/:layers.

```lua
local anim = node:animator()
anim:crossfade(node.vel:length() > 4 and "run" or "walk", 0.15)
```

### `node:children`

An array of this node's child handles.

### `node:destroy`

node:destroy() — remove this node and its children (same as destroy(node)). The classic pickup: onTriggerEnter → award score → node:destroy().

### `node:find`

node:find("Muzzle") — the first descendant (any depth) with that name, or nil.

### `node:getchild`

node:getchild("Gun") — the first child with that name (a node handle), or nil.

### `node:getparent`

The parent node handle, or nil (same as node.parent).

### `node:getscript`

node:getscript("health") — a script handle for that script on this node, or nil. Read/write its state, call its methods, reach .node / .params.

### `node:hasTag`

node:hasTag("enemy") — whether the node carries that exact tag. The classic hit-filter: local hit = raycast(...) if hit and hit.node and hit.node:hasTag("enemy") then ... end

### `node:removeTag`

node:removeTag("burning") — remove a tag (no-op when absent).

### `node:setCelestial`

node:setCelestial{mu=…, bodyRadius=…, soi=0, parent="Sun", a=…, e=…, i=…, m0=…, atmoColor={r,g,b}, atmoHeight=…, atmoDensity=…, clouds=…, luminosity=…, starColor={r,g,b}, occluderRadius=…} — set (creating if absent) the node's CelestialBody. camelCase fields; colors take {r,g,b}. occluderRadius = occlusion culling: the solid-core radius geometry never pierces — terrain chunks fully behind it skip their draw calls (keep it below the deepest cave/dig; 0 = off).

### `node:setMaterial`

node:setMaterial{color={r,g,b}, emissive={r,g,b}, emissiveStrength=…, unlit=true, texture="…", alpha=…, …} — set (creating if absent) the node's Material. texture also takes a live render target: "rt:<name>".

```lua
-- setup-time; use setShaderParam for per-frame values
node:setMaterial{ unlit = true, emissive = {1, 0.45, 0.15}, emissiveStrength = 2.5 }
```

### `node:setPrimitive`

node:setPrimitive("Sphere" [, {r,g,b}]) — make the node a primitive (Cube/Sphere/Capsule/Plane).

### `node:setShaderParam`

node:setShaderParam("glow", 2.5) / node:setShaderParam("nose", x, y, z) — drive a .flsl uniform on this node every tick (a GPU uniform write, never a recompile). Targets the node's Material shader, or its UI element's `stage ui` shader (the navball pattern: a script feeds an instrument's uniforms each tick). Unset lanes are 0.

```lua
-- a live uniform write: safe every tick, never recompiles
node:setShaderParam("cell", math.floor(time * 8) % 16)
```

### `node:setTerrain`

node:setTerrain(id) — make the node a Terrain volume with that id; fill it with terrain.generatePlanet(id, opts).

### `node:setTerrainGen`

node:setTerrainGen(opts) — attach an ON-DEMAND generation spec (the same opts table terrain.generatePlanet takes): the body's field generates in the background when something first approaches, so no field file is needed at all — a rolled galaxy is playable instantly and unvisited worlds cost one scene node. Player edits saved under terrain.saveDir take priority over regeneration. nil clears.

### `node:sound`

node:sound() — the handle for this node's Audio Source component. :play() (restarts), :stop(), :pause(), :resume(), :setClip("audio/x.ogg"), :seek(secs), :isPlaying(), :position(). Tunables (volume/pitch/distances/…) live on node:getcomponent("AudioSource").

## vectors, directions & easing

### `dirFromYaw`

dirFromYaw(yaw [, pitch]) — the unit direction those angles face: the inverse of yawOf/pitchOf. Without a pitch you get the ground direction, which is what movement wants; with one you get a camera's view direction.

```lua
-- the yaw/pitch -> direction pair, with the right signs
local look = dirFromYaw(node.yaw, node.pitch)
node.pos = head - look * distance   -- an orbit camera, in one line
```

### `dirTo`

dirTo(from, to) — the UNIT direction from one thing to another. Both may be a vec3, a {x=,y=,z=} table or a NODE handle, so dirTo(node, target) is the whole sentence. Same point twice → vec3(0,0,0), never a NaN.

```lua
local aim = dirTo(node, find("Enemy"))
spawn("Bullet", node.pos + aim * 1.5, function(b) b.vel = aim * 60 end)
```

### `distance`

distance(a, b) — distance between two points: vec3/vec2 values, {x=,y=,z=} tables, or NODE handles (distance(node, target) just works). Also distance(x1,y1,z1, x2,y2,z2) for raw numbers.

### `ease`

ease(a, b, rate, dt) — frame-rate-independent exponential ease: `a` covers a rate-dependent FRACTION of the remaining distance each second, so 30 fps and 240 fps feel identical. Numbers or vectors. rate <= 0 snaps. This is what a camera's "smoothing" knob is; three shipped camera scripts each defined it privately before it lived here.

```lua
-- the same feel at 30 fps and at 240 (this is what "smoothing" is)
function lateUpdate(node, dt)
  node.pos = ease(node.pos, target.pos + offset, params.smoothing, dt)
end
```

### `lookRotation`

lookRotation(dir [, up]) -> yaw, pitch, roll — the angles that face `dir`, WITHOUT applying them (node:lookAt applies them). Three returns, so `node.yaw, node.pitch, node.roll = lookRotation(f, up)` is one line. No up = roll 0.

```lua
-- the angles, without applying them
node.yaw, node.pitch, node.roll = lookRotation(forward, node.up)
```

### `moveTowards`

moveTowards(node, target, maxDelta) — walk a node toward a WORLD point at a speed, never overshooting it. Pass `speed * dt`. Returns true once it has arrived, so `if moveTowards(node, goal, s * dt) then` is the whole patrol step. Also spelled node:moveTowards(target, maxDelta).

```lua
-- a patrol, in two lines: it returns true on arrival
if node:moveTowards(waypoints[i], params.speed * dt) then
  i = i % #waypoints + 1
end
```

### `node.worldPos`

The node's position in WORLD space as a vec3, composed up the parent chain (read-only; node.worldX/worldY/worldZ are the components). node.x/y/z are LOCAL — comparing those against a world target is how a unit under a container walks past its destination and keeps going.

```lua
-- x/y/z are LOCAL; this is where it really is
if node.worldPos:distance(order) < params.arrive then arrived() end
```

### `node:distanceFlat`

node:distanceFlat(other [, up]) — distance ignoring the up axis (default +Y): the "have I arrived?" test for anything that walks on ground it doesn't control the height of. Pass an up for a planet.

### `node:distanceTo`

node:distanceTo(other) — distance to a node or a point, measured in WORLD space, which is the answer people mean. `distance(a, b)` compares LOCAL positions — correct right up until one of the two is parented, and then quietly about the wrong frame.

```lua
-- WORLD space, so a unit under a container measures the real gap
if node:distanceTo(player) < params.aggro then chase(player) end
```

### `node:lookAt`

node:lookAt(target [, up]) — point this node at another node or a world point. Sets yaw + pitch and leaves roll alone; pass an `up` and it sets the roll too, to whatever puts that up over the node's head (a level horizon on a planet — the twenty-line undo-yaw-then-pitch dance, in one call). Measured in WORLD space on both ends.

```lua
-- point at a node or a world point; the up makes the horizon level
node:lookAt(find("Enemy"))
node:lookAt(aimPoint, node.up)   -- roll set too, for a planet camera
```

### `node:moveTowards`

node:moveTowards(target, maxDelta) — the method spelling of moveTowards(node, …). World-space and placed through the parent inverse, so a node under a container arrives where you actually pointed.

### `node:setWorldPos`

node:setWorldPos(v) — put this node at a WORLD point, whatever it is parented to, without deriving the parent inverse by hand. Goes through the componentwise TRS inverse, so it stays exact under a MIRRORED (negative-scale) parent, where a matrix decomposition puts the flip on the wrong axis.

```lua
-- land on a world point whatever this node is parented to
node:setWorldPos(hit.node:toWorld(vec3(0, 1, 0)))
```

### `node:toLocal`

node:toLocal(v) — the inverse of node:toWorld: a world point expressed in this node's frame.

### `node:toWorld`

node:toWorld(v) — a point in this node's own frame, converted to world space: its position, rotation AND scale, composed up the whole parent chain. "Where is the muzzle?" is gun:toWorld(vec3(0, 0, -1.2)).

```lua
-- composes position, rotation AND scale up the whole parent chain
local muzzle = gun:toWorld(vec3(0, 0, -1.2))
spawn("Bullet", muzzle, function(b) b.vel = gun:worldForward() * 60 end)
```

### `node:turnTowards`

node:turnTowards(target, maxRadians) — turn toward something by at most that much, the SHORT way round (the ±pi seam is handled). Pass `rate * dt` for a frame-rate-independent turn. A node handle or a world point is somewhere to face; any other vector is taken as a DIRECTION, so node:turnTowards(node.vel, 6 * dt) steers a unit to face where it is going. A zero-length direction leaves the facing alone.

```lua
-- swing round at a rate instead of snapping. Short way, always.
node:turnTowards(find("Enemy"), params.turn_rate * dt)
node:turnTowards(node.vel, 6 * dt)   -- or: face where you're going
```

### `node:worldForward`

node:worldForward() — the node's forward AFTER the parent chain. node.forward is the LOCAL one: a gun barrel parented to a swinging arm points where the ARM says, so shooting along node.forward misses. Also node:worldRight() and node:worldUp().

### `node:worldRight`

node:worldRight() — the node's +X axis after the parent chain.

### `node:worldUp`

node:worldUp() — the node's +Y axis after the parent chain (not the same as node.up, which is the body's −gravity up).

### `pitchOf`

pitchOf(dir) — the pitch that faces along a direction, positive looking up. asin, clamped, so a denormalised vector can't produce a NaN.

### `smoothDamp`

smoothDamp(current, target, vel, smoothTime, dt) -> value, vel — a critically-damped spring: unlike ease it has MOMENTUM, so a follow keeps moving for a moment after the target stops. Lua has no reference parameters, so the velocity comes back as the second return: camX, camVX = smoothDamp(camX, wantX, camVX, 0.25, dt). Numbers or vectors.

```lua
-- a follow with momentum: it keeps moving after the target stops
camX, camVX = smoothDamp(camX, target.worldX, camVX, 0.25, dt)
```

### `vec2`

vec2(x, y) — a 2-vector value (UI/screen math), same operators and methods as vec3 (minus cross).

### `vec2.x`

The vector's X.

### `vec2.y`

The vector's Y.

### `vec2:distance`

vec2:distance(other) — the distance between two 2-D points.

### `vec2:dot`

vec2:dot(other) — the dot product; the cosine of the angle when both are unit length.

### `vec2:length`

vec2:length() — how long the 2-D vector is.

### `vec2:lengthSquared`

vec2:lengthSquared() — length without the square root, for comparisons.

### `vec2:lerp`

vec2:lerp(other, t) — a straight-line blend from this (t = 0) to other (t = 1).

### `vec2:normalized`

vec2:normalized() — a unit-length copy, pointing the same way. Zero stays zero rather than becoming a NaN.

### `vec3`

vec3(x, y, z) — a 3-vector VALUE with real operators: a + b, a - b, v * 2, -v, a == b. Methods: :length(), :lengthSquared(), :normalized(), :dot(o), :cross(o), :lerp(o, t), :distance(o), :flatten(up), :withX/:withY/:withZ(n), :rotatedY(rad), :rotatedAround(axis, rad), :towards(o, maxDelta), :angleTo(o). vec3() = zero, vec3(s) = splat, vec3(other) = copy. Anything that takes a vector also takes a {x=,y=,z=} table or a node handle.

```lua
local v = vec3(1, 0, 0) * 5 + vec3(0, 2, 0)   -- real operators
log(v:length(), v:normalized(), v:dot(node.forward))
```

### `vec3.x`

The vector's X. Vectors are values, not handles — writing v.x = 5 changes that vector, not whatever it came from.

### `vec3.y`

The vector's Y.

### `vec3.z`

The vector's Z.

### `vec3:angleTo`

v:angleTo(other) — the unsigned angle between two directions, in radians. Clamped before the acos, so parallel vectors give 0 and a zero vector gives 0 — never a NaN.

### `vec3:cross`

vec3:cross(other) — a vector perpendicular to both, right-handed. The way to build a basis, or to ask which side of a plane something is on.

### `vec3:distance`

vec3:distance(other) — the distance between two points. Reads better than (a - b):length() and does the same thing.

### `vec3:dot`

vec3:dot(other) — the dot product. With unit vectors it is the cosine of the angle between them: node.forward:dot(toEnemy) > 0.7 is a 45° cone in front.

### `vec3:flatten`

v:flatten(up) — the part of v that lies in the plane PERPENDICULAR to up, renormalised. THE planet-safe move: "forward along the ground" is dirFromYaw(node.yaw):flatten(node.up) whatever the local vertical is, and on a flat world :flatten() (default +Y) is the familiar "drop the Y". Straight up or down leaves nothing in the plane → vec3(0,0,0), never a NaN.

```lua
-- "forward along the ground" — on a flat world AND on a planet
local up = node.up or vec3(0, 1, 0)
local fwd = dirFromYaw(node.yaw):flatten(up)
local right = fwd:cross(up)
```

### `vec3:length`

vec3:length() — how long the vector is. The distance form of a difference: (b - a):length().

### `vec3:lengthSquared`

vec3:lengthSquared() — length without the square root. Compare distances with it (d2 < r*r) and skip the expensive part.

### `vec3:lerp`

vec3:lerp(other, t) — a straight-line blend, t from 0 (this) to 1 (other). The one-liner behind smooth camera and marker movement.

### `vec3:normalized`

Unit-length copy (zero stays zero).

### `vec3:rotatedAround`

v:rotatedAround(axis, rad) — Rodrigues rotation about ANY axis, which is what a planet camera's yaw actually is (about the LOCAL up, not about +Y).

### `vec3:rotatedY`

v:rotatedY(rad) — spun about world +Y (the yaw of a flat world). For any other axis use v:rotatedAround(axis, rad).

### `vec3:towards`

v:towards(other, maxDelta) — step toward another point without ever overshooting it: math.approach, for positions. Pass `speed * dt`.

### `vec3:withY`

v:withX(n) / v:withY(n) / v:withZ(n) — the same vector with one component replaced. node.vel:withY(0) keeps your fall speed out of a horizontal speed clamp.

### `yawOf`

yawOf(dir) — the yaw that faces along a direction. This is atan2(-x, -z) (engine forward is −Z), once and with the right signs. Zero direction → 0.

```lua
-- which way is that? (atan2(-x, -z), once and correctly)
local heading = math.deg(yawOf(node.vel))
```

## scene lookups & raycast

### `capsulecast`

capsulecast(origin, dir, radius, halfHeight, max [, opts]) — the player-shaped sweep: "can I actually move there", asked with the shape that will be moving. Upright along the capsule's own axis, matching how the solver keeps a capsule body aligned, so the cast and the move agree.

### `find`

find("Player") — the first node in the scene with that name (a node handle), or nil.

```lua
-- cache in start; find() every frame is wasteful
function start(node) player = find("Player") end
```

### `findAll`

findAll("Coin") — an array of every node with that name.

### `findScript`

findScript("GameManager") — a script handle for the first node anywhere running that script (the manager pattern), or nil. Call its methods / read its state.

### `findScriptInScene`

Alias of findScript(kind).

### `findScripts`

findScripts(kind) — EVERY node carrying that script, as script handles in scene order. Pair with net.isMine to pick the local player out of many avatars: for _, s in ipairs(findScripts("third_person")) do if net.isMine(s.node) then ... end end

### `findTagged`

findTagged("enemy") — EVERY node carrying that tag (Inspector tag chips / node:addTag), as node handles in scene order. Empty table when none; findTagged("enemy")[1] grabs the first.

```lua
for _, e in ipairs(findTagged("enemy")) do
  if distance(node, e) < 10 then e:destroy() end
end
```

### `hit.nx`

Contact normal X (unit, out of the hit surface).

### `hit.ny`

Contact normal Y.

### `hit.nz`

Contact normal Z.

### `hit.x`

Contact point X (world).

### `hit.y`

Contact point Y (world).

### `hit.z`

Contact point Z (world).

### `overlapSphere`

overlapSphere(center, radius [, opts]) — everything inside a sphere, DEEPEST overlap first, as hit tables ({x,y,z, nx,ny,nz, distance, node}). Reports static geometry AND body hulls. opts takes { exclude = node, layers = {"Enemies"} }. The blast-radius / "what is in this area" query.

### `raycast`

raycast(origin, dir, max [, ignore]) — or raycast(ox,oy,oz, dx,dy,dz, max [, ignore]). Cast a ray against the terrain + mesh colliders AND every physics body (players, crates). Returns a hit {x,y,z, nx,ny,nz, distance, node} or nil — node is the hit body's node handle (nil for static geometry). Your own node's body is excluded; pass a node as `ignore` to skip its body too. The last arg can instead be an options table: raycast(..., { ignore = target, layers = {"Ground"} }) — layers (name or array, Project Settings → Layers) filters what the ray can hit; a misspelled layer is an error. Use for ground checks, line-of-sight, shooting.

```lua
local hit = raycast(node.pos, vec3(0, -1, 0), params.ground_ray)
if hit then log("ground at " .. hit.y) end
```

### `spherecast`

spherecast(origin, dir, radius, max [, opts]) — the first thing a moving BALL of that radius would hit, or nil. A raycast that can't slip through a gap narrower than the thing you are actually moving.

## references — wire nodes in the Inspector

### `componentref`

defaults = { body = componentref("RigidBody") } — the param binds to that COMPONENT on the wired node: params.body is a component handle directly (params.body.friction = 0.05). Components: RigidBody, PointLight, Camera, ParticleSystem, UiElement, UiSlider, UiLayer. nil while unwired/invalid.

### `noderef`

defaults = { target = noderef() } — a NODE REFERENCE param: the Inspector shows a node picker (or drag a node from the Hierarchy onto it) and the script reads params.target as a node handle (nil while unwired). The preferred way to point a script at a specific node — no find() calls.

### `scriptref`

defaults = { hp = scriptref("health") } — the param binds to that SCRIPT on the wired node: params.hp is a script handle directly (call its functions, read its state). The Inspector only lists nodes carrying the script. nil while unwired/invalid.

## input — keyboard & mouse

### `input`

Player input (play mode). input.key/pressed/axis/mouse/button — make interactive games.

### `input.action`

input.action("Jump") — true while a NAMED action is held, from any of its bindings (key, mouse button, pad button, trigger). Define actions in Project Settings → Input; the list there is scanned from your scripts, so a name you type here shows up ready to bind. Prefer actions over input.key: they work on a gamepad, the player can rebind them, and they're what multiplayer replicates.

```lua
-- actions, not raw keys: rebindable, gamepad-ready, replay-safe
if input.action("jump") and node.grounded then
  node.vel = node.vel + node.up * params.jump
end
```

### `input.actions`

input.actions() — every action name in the map, for drawing an in-game controls screen.

### `input.aimPitch`

The active camera's world pitch (radians), captured with the input snapshot.

### `input.aimYaw`

The ACTIVE camera's world yaw (radians), captured with the input snapshot — use it for camera-relative movement (in multiplayer it rides the input command, so server + prediction replay see exactly your view angle). nil without an active camera.

### `input.axis`

input.axis("a", "d") — returns -1/0/1 from a negative/positive key pair (e.g. strafing).

### `input.axis1`

input.axis1("Zoom") — a named 1D axis in -1..1 (triggers, wheel, or a key pair).

### `input.axis2`

local x, y = input.axis2("Move") — a named 2D axis clamped to the unit disk. Reads identically on WASD and on a stick; deadzone and SOCD are handled for you.

```lua
local mx, my = input.axis2("move")
node.pos = node.pos + (node.right * mx + node.forward * my) * params.walk * dt
```

### `input.bindingsOf`

input.bindingsOf("Jump") — an action's bindings as printable chips ("⌨ Space", "🎮 South").

### `input.buffered`

input.buffered("Punch", 4) — was it pressed within the last 4 TICKS and not yet consumed? The input buffer: a player who hits Punch a couple of frames before recovery ends still gets the punch. Pair with input.consume so it fires once. fixedUpdate only.

### `input.button`

input.button(0) — true while a mouse button is held (0 left, 1 right, 2 middle).

### `input.cancelRebind`

input.cancelRebind() — abandon a rebind in progress, leaving the old binding alone.

### `input.clicked`

input.clicked(0) — true only on the frame a mouse button goes down.

### `input.commitRebind`

input.commitRebind() — accept the captured binding. Returns false if nothing was captured yet.

### `input.consume`

input.consume("Punch", 4) — spend a buffered press. Without it a 4-tick buffer fires your attack on all four ticks.

### `input.dir`

input.dir() — the current numpad direction from "Move", from the character's point of view: 7 8 9 / 4 5 6 / 1 2 3, where 5 is neutral and 6 is forward.

### `input.dirHeldTicks`

input.dirHeldTicks(4) — consecutive ticks a numpad direction has been held. Build your own charge or leniency rules on it.

### `input.facing`

input.facing() — which way this player's character is facing, as -1 or 1. The fighter layer mirrors directional input by it, so "forward" means toward the opponent on both sides of the screen.

### `input.heldSecs`

input.heldSecs("Charge") — seconds the action has been continuously held (0 when up). Hold-to-charge without your own timer.

### `input.justPressed`

input.justPressed("Punch") — true only on the frame (or tick, inside fixedUpdate) the action goes down.

### `input.justReleased`

input.justReleased("Block") — true only on the frame/tick the action goes up.

### `input.key`

input.key("w") — true while the key is held. Names: a-z, 0-9, space, enter, shift, ctrl, alt, left/right/up/down, escape, tab.

### `input.lockMouse`

input.lockMouse() — pin the cursor to the window center and hide it (FPS / free-look mouselook without holding a button). Read motion with input.mouse_delta(). Released on Stop.

### `input.motion`

input.motion("qcf") — has a fighting-game motion just been completed? Seeded set: qcf, qcb, dp, rdp, hcf, hcb, dd, ff, bb, chargeF, chargeU (edit them in input.ron). Combine with input.buffered for a special: `if input.motion("qcf") and input.buffered("Punch", 4) then`. fixedUpdate only.

### `input.mouse`

local x, y = input.mouse() — cursor position in pixels.

### `input.mouse_delta`

local dx, dy = input.mouse_delta() — mouse movement since last frame.

### `input.padAxis`

input.padAxis(1, "leftx") — read a pad axis raw, -1..1, past the action map. Same diagnostic purpose as input.padButton; bind through actions for real gameplay.

### `input.padButton`

input.padButton(1, "a") — read a pad button RAW, straight past the action map. Deliberately unmediated: this is what distinguishes "your pad works, your bindings are wrong" from "your pad is not here".

### `input.padCount`

input.padCount() — how many gamepads are connected. The quick check behind a "press a button to join" prompt.

### `input.pads`

input.pads() — every gamepad the engine has enumerated: { index, name, connected }. Show it in your options screen; "the pad isn't listed" and "the pad is listed but nothing is bound" are different problems and only this can tell them apart.

### `input.pendingRebind`

input.pendingRebind() — the captured chip text once something has been pressed, an EMPTY string while still waiting, or nil when no rebind is running. Enough for a menu to show "press any button…" and then the result.

### `input.player`

input.player(2) — the same input API bound to another LOCAL player (1-based). Two characters can run the same script: pass the slot as a param and use `local me = input.player(params.player)`. Set the count in Project Settings → Input. Sharing ONE keyboard: scope a binding to a player (right-click its chip) so a single action name can be J for P1 and 1 for P2 — pads sort themselves out already.

### `input.popContext`

input.popContext("menu") — remove an input layer. Returns whether one was removed.

### `input.pressed`

input.pressed("space") — true only on the frame the key goes down (an edge).

### `input.pushContext`

input.pushContext("menu", { priority = 100, consume = true, enabled = { "Pause" } }) — a consuming layer swallows every action it doesn't list, so a menu or dialogue eats movement without the player controller knowing. Pop it with input.popContext("menu").

### `input.released`

input.released("space") — true only on the frame the key goes up (an edge).

### `input.scroll`

input.scroll() — mouse wheel delta this frame.

### `input.setFacing`

input.setFacing(-1) — mirror this player's directions after a cross-up, so motion("qcf") keeps meaning "toward the opponent". The engine has no opinion about who faces where; the game sets it.

### `input.setMouseLocked`

input.setMouseLocked(true/false) — lock or unlock the mouse from a boolean (e.g. a menu toggle).

### `input.startRebind`

input.startRebind("Jump", "pad") — arm press-to-bind from a settings menu. Poll input.pendingRebind() for the captured chip, then input.commitRebind(). Filters: "keyboard", "pad", "axis", or nil for any button. Escape always cancels.

### `input.typed`

input.typed() — the CHARACTERS entered this frame, as a string, resolved by the OS keyboard layout (a paste folded in). Not the same question as input.pressed: that one is physical, so "q" is the key where Q sits on QWERTY and types `a` on AZERTY. Never contains control characters — Enter and Backspace stay actions. Empty while a UI text field has focus, because the field ate them.

### `input.unlockMouse`

input.unlockMouse() — release the cursor back to the desktop and show it again.

## drawing — draw.*

### `draw`

The GAME's telegraph layer — 3D lines/shapes and screen-space rects, circles and text that SHIP with your game. gizmo.* is the debug-only twin that never appears for a player.

### `draw.box`

draw.box(cx,cy,cz, hx,hy,hz, yaw, r,g,b [,a]) — a yaw-rotated wireframe box from half-extents. Trigger volumes, build footprints, an attach point.

### `draw.circle`

draw.circle(x, y, radius, r,g,b [, a]) — a filled circle in screen pixels, x/y its CENTRE. draw.circleOutline(..., [px]) is the hollow twin. Same immediate-mode rules as draw.rect: over the scene, over the HUD, one frame each.

```lua
-- x, y is the CENTRE
draw.circle(mx, my, 6, 0.3, 1.0, 0.5, 0.9)
draw.circleOutline(mx, my, 18, 0.3, 1.0, 0.5, 0.5, 2)
```

### `draw.circleOutline`

draw.circleOutline(x, y, radius, r,g,b [, a] [, px]) — a hollow circle, `px` thick (default 2).

### `draw.cone`

draw.cone(bx,by,bz, dx,dy,dz, radius, height, r,g,b [,a]) — a SOLID cone: base disc at b, apex `height` along the unit direction d. Gizmo arrowheads, thruster plumes, direction markers.

### `draw.disc`

draw.disc(cx,cy,cz, nx,ny,nz, r0, r1, r,g,b [,a]) — a filled annulus around normal n (r0 = inner, r1 = outer; r0 = 0 gives a full disc). Rotation gizmo bands, ground markers.

### `draw.line`

draw.line(x1,y1,z1, x2,y2,z2, r,g,b [, a]) — queue one world-space 3D line for THIS frame (immediate mode: re-draw every lateUpdate — the camera pass — while wanted). Drawn OVER the scene, never occluded — the KSP-style map draws its orbit conics with these.

### `draw.rect`

draw.rect(x, y, w, h, r,g,b [,a] [,radius]) — a filled rectangle in SCREEN PIXELS, in input.mouse()'s space. An RTS marquee is just the two corners you dragged between — the 3D version has to be projected onto a ground plane, which fights the camera angle and misses whatever the plane doesn't cross.

### `draw.rectOutline`

draw.rectOutline(x, y, w, h, r,g,b [,a] [,thickness]) — the hollow twin of draw.rect. The last number is the border thickness rather than a corner radius.

### `draw.ring`

draw.ring(cx,cy,cz, nx,ny,nz, radius, r,g,b [,a]) — a circle around normal n at c. Range rings, selection circles, an area-of-effect telegraph.

### `draw.sphere`

draw.sphere(cx,cy,cz, radius, r,g,b [,a]) — three rings, i.e. a wireframe ball. Cheap enough to draw per-frame for every marker on screen.

### `draw.text`

draw.text(x, y, s, size, r,g,b [, a] [, align]) — a string on the SCREEN, in the pixels input.mouse() reports, without building a UI tree: a damage number, a frame-time readout, the count under a selection box. The engine measures and lays out the glyphs with the same font stack ui.make uses. align is "left" (default) | "center" | "right", and x is that edge. Immediate mode: re-draw it every frame you want it.

```lua
-- a HUD with no UI tree; align says which edge x is
draw.text(24, 24, "HP " .. hp, 22, 1, 0.4, 0.4)
draw.text(w - 24, 24, string.format("%.0f fps", 1 / dt), 18, 1, 1, 1, 0.7, "right")
```

### `draw.tri`

draw.tri(x1,y1,z1, x2,y2,z2, x3,y3,z3, r,g,b [,a]) — one filled triangle. The raw primitive under the solid shapes, for when you want your own.

## the web — http.*, json.*

### `http.cancelAll`

http.cancelAll() — forget every pending callback. Stop and scene.load do this for you: a callback closes over nodes from the scene that asked, and delivering it into a fresh session is how one run inherits the previous one's network.

### `http.delete`

http.delete(url [, opts], function(res) end) — as http.get, with DELETE.

### `http.get`

http.get(url [, opts], function(res) end) — fetch a URL. NON-BLOCKING: the callback runs on a later tick on the MAIN thread, so it is safe to touch nodes from it and a slow server can never stall a frame. opts = { headers = {...}, timeout = 10, json = true }. res = { ok, status, body, json, error } — `ok` is a 2xx with no error; a 404 still hands you `body`, because that is where an API explains itself. Play only.

```lua
-- non-blocking: the callback runs on a later tick, on the main thread
http.get(params.api .. "/me/cards", {
  headers = { Authorization = "Bearer " .. token },
}, function(res)
  if not res.ok then return log("failed: " .. tostring(res.error)) end
  for _, card in ipairs(res.json.cards or {}) do addCard(card) end
end)
```

### `http.inFlight`

http.inFlight() — how many requests are still waiting on a reply. Up to 8 may be in flight and 20 may start per second; past that, calls fail fast with res.error and the cap announces itself once in the Console. A cap you are hitting is nearly always a request inside update().

### `http.post`

http.post(url, body [, opts], function(res) end) — same rules as http.get, plus a body: a STRING is sent as-is, a TABLE is encoded as JSON for you. http.put and http.delete round out the set.

```lua
-- a TABLE body is sent as JSON; no json.encode dance needed
http.post(params.api .. "/me/loadout", { deck = deckId }, function(res)
  if not res.ok then log("the server said no: " .. res.body) end
end)
```

### `http.put`

http.put(url, body [, opts], function(res) end) — as http.post, with PUT.

### `json.decode`

json.decode(s) -> value, err — parse JSON. Bad input returns nil AND a message rather than raising: a reply from someone else's server is data, not a bug in your script. JSON null becomes nil, so a null field reads exactly like a missing one.

```lua
-- bad input is a VALUE, not an error
local save, why = json.decode(text)
if not save then return log("corrupt save: " .. why) end
```

### `json.encode`

json.encode(value) — a Lua value as a JSON string. A table with a [1] is an ARRAY, anything else is an object (that is the only rule Lua's single table type can support). http.post takes a table body directly, so you rarely need this by hand.

### `openUrl`

openUrl(url) — open an http:// or https:// address in the player's own browser. The device-code sign-in flow needs it: the player approves the pairing on your real site, so the game never sees a password and needs no secret baked into it. Play only; if the platform refuses, the URL is logged instead so the player can still get there.

```lua
-- the player approves the pairing on your real site
openUrl(res.json.verify_url)
```

## the player's account — account.*

### `account`

The signed-in player: account.signIn(), account.player(), and http verbs that carry the session. A script asks for a PLAYER, never for a token, and the server decides what that player owns.

### `account.cancel`

account.cancel() — abandon a sign-in in progress (the player pressed Escape). Harmless at any other time.

### `account.code`

account.code() — while state() is "waiting": { code = "WXYZ-9999", url = "...", expiresIn = 900 }. Show the code and send them to the url (openUrl does it) — that pairing is what the player approves. nil at any other time.

### `account.delete`

account.delete("/games/mygame/saves/slot1", function(res) end) — remove something from Floptle Cloud.

### `account.error`

account.error() — why the last sign-in failed, as a sentence you can put on screen. nil unless state() is "failed".

### `account.get`

account.get("/wallet", function(res) end) — a Floptle Cloud call with the player's bearer token attached for you. Takes a PATH, not a URL: there is exactly one host it can reach, which is what makes attaching a token to it safe. Bare paths get the /api/floptle/v1 prefix; /userinfo and /oauth/* stay at the root. res is the same table http.* gives you.

### `account.inFlight`

account.inFlight() — how many account calls are still waiting on a reply (cap 6). A spinner, or a guard against firing the same request every frame.

### `account.player`

account.player() — { id, name, email, tier } once signed in, else nil. There is deliberately no way to read the access token: a shipped game's Lua is readable, so anything a script can hold a player can read out of the file.

### `account.post`

account.post("/games/mygame/events", { event = "boss_killed", event_id = id }, function(res) end) — report what HAPPENED and let the server decide what it is worth. A table body is sent as JSON. There is no endpoint that credits currency directly, by design: anything a client can announce, a modified client can announce.

### `account.put`

account.put("/games/mygame/saves/slot1", { data = t, expected_version = v }, function(res) end) — a cloud save. expected_version is optimistic concurrency: send the version you last read and a stale write gets 409 instead of silently clobbering the player's other machine.

### `account.signIn`

account.signIn() — begin signing the player in to their Foverse account (fopull.com). Returns IMMEDIATELY; watch account.state() and draw account.code(). The engine drives the OAuth device flow in Rust — the player approves in their browser, so the game never sees a password and never holds a token. Play only.

### `account.signOut`

account.signOut() — forget the session NOW, then clear the keyring and revoke the refresh token in the background. In that order on purpose: a player who presses Sign Out is signed out whether or not the network agrees.

### `account.state`

account.state() — "signedOut" | "starting" | "waiting" | "signedIn" | "failed". Polled rather than called back, because signing in takes as long as a person takes to pick up their phone and a sign-in screen is redrawing anyway.

## game UI — text, buttons & hooks

### `cancelled`

function cancelled(node) — UI hook: the UiCancel action (Escape / B) while this element has focus. Back out of a screen from the element the player is on.

### `changed`

function changed(node) — UI hook: a text field's value changed (typing, paste, backspace). Once per frame however many keystrokes landed. Read node.text.

### `clicked`

function clicked(node) — UI button hook: fires when this node's element (with 'button' on) is pressed AND released on it. Style states in Lua; no imposed look.

### `color`

color(r, g, b [, a]) — a colour, 0..1 per channel, alpha 1 by default. Also color(gray [, a]) and color(other [, a]) to copy with a new alpha. It's a plain table {r,g,b,a} (also [1]..[4]) so it prints, saves and compares. Assign it whole: el.fill = color(1, 0.85, 0.35), el.textColor, el.borderColor, el.tint, el.groupTint, el.caretColor.

### `color.hex`

color.hex("#ff8800") / color.hex("ff8800aa") — 6 or 8 hex digits. A 3-digit shorthand is refused rather than guessed at.

### `color.lerp`

color.lerp(a, b, t) — blend two colours per channel, t clamped to 0..1.

### `dragCancel`

function dragCancel(node) — UI hook: a drag was released over nothing. Put the item back; a half-finished gesture must not leave it stuck to the cursor.

### `dragEnter`

function dragEnter(node) — UI hook: a drag moved over this `drop target`. Pair with `dragLeave`; highlight the slot here.

### `dragLeave`

function dragLeave(node) — UI hook: the drag moved off this drop target.

### `dragMove`

function dragMove(node) — UI hook: fires every frame of a drag on the SOURCE. Use input.mouse() / node:uiRect() to position whatever you're showing.

### `dragOver`

function dragOver(node) — UI hook: fires every frame a drag rests over this drop target.

### `dragStart`

function dragStart(node) — UI hook: a `draggable` element has been picked up (the pointer travelled far enough that it isn't a click). The engine does NOT move the element — draw the drag however your game wants.

### `dropped`

function dropped(node) — UI hook: fires on BOTH ends of a completed drag — the target (which now has it) and the source (which gave it away). `ui.dragging()` and `ui.dropTarget()` name the pair.

### `el.border`

Shape border thickness (design units).

### `el.cell`

Spritesheet cell index the image shows (set per frame for sprite animation).

### `el.fillA`

Shape fill alpha 0..1.

### `el.fillB`

Shape fill blue 0..1.

### `el.fillG`

Shape fill green 0..1.

### `el.fillR`

Shape fill red 0..1.

### `el.height`

Height (same rules as width).

### `el.opacity`

Multiplies every color the element draws, 0..1.

### `el.posX`

Free position X / Pin offset X (design units).

### `el.posY`

Free position Y / Pin offset Y (design units).

### `el.radius`

Shape corner radius (design units).

### `el.scrollY`

Scroll-view position, design units (0 = top; the wheel drives it too, clamped to the content). Present only on elements with the scroll-view option.

### `el.textA`

Text color alpha 0..1.

### `el.textB`

Text color blue 0..1.

### `el.textG`

Text color green 0..1.

### `el.textR`

Text color red 0..1.

### `el.textSize`

Text glyph size (design units; ignored while fit is on).

### `el.tintA`

Image tint alpha 0..1.

### `el.tintB`

Image tint blue 0..1.

### `el.tintG`

Image tint green 0..1.

### `el.tintR`

Image tint red 0..1.

### `el.visible`

Shown (1/0; assign true/false).

### `el.width`

Width in the axis's sizing mode (px value, % fraction, or grow weight). Absent (nil) on a fit axis; writing one makes it fixed px.

### `focusEnter`

function focusEnter(node) — UI hook: keyboard/gamepad focus arrived here. What focus LOOKS like is your style's `focus` block; this is for the rest (a sound, a preview, a description panel).

### `focusExit`

function focusExit(node) — UI hook: focus left this element.

### `hoverEnd`

function hoverEnd(node) — UI hook: the pointer left this node's clickable element.

### `hoverStart`

function hoverStart(node) — UI hook: the pointer entered this node's clickable element. Pair with hoverEnd.

### `layer.designHeight`

Design units that span the window height.

### `layer.enabled`

Master switch (1/0; assign true/false) — an off layer draws nothing.

### `layer.worldSpace`

1 = a panel inside the 3D world at this node's transform; 0 = a screen overlay.

### `layer.z`

Draw order: lowest z first.

### `node.index`

Which row of a UI repeater this node is, 0-based — nil on anything a repeater didn't spawn, so `if node.index then` is a fine "am I a row". Read the count with getcomponent("UiElement").count on the container.

### `node.text`

A UI element's label text — read/write; numbers coerce (hpLabel.text = 42). nil on nodes without UI text; writing to a UI element without a text spec creates one.

### `pressed`

function pressed(node) — UI hook: LMB went down on this node's clickable element.

### `released`

function released(node) — UI hook: LMB came back up (on or off the element).

### `slider.max`

Range end.

### `slider.min`

Range start.

### `slider.value`

Current value (clamped to min..max at draw time).

### `submitted`

function submitted(node) — UI hook: Enter (UiSubmit) in a focused TEXT FIELD. Read the value with node.text. A field fires this instead of `clicked`, so a field inside a button doesn't run the button.

### `ui.bind`

ui.bind(node, "property", function() ... end) — say the relationship once instead of writing an update() that keeps it true. The engine calls the function once a frame, after every update, and writes what it returns: a string or number to "text", a color(...) to a colour field, a number/boolean to any component field (the component is picked by which one actually has that field, so "value" finds UiSlider). Re-binding the same property replaces. A binding whose node is gone is dropped silently; one that throws is dropped after reporting once.

### `ui.changed`

ui.changed(element) — a text field's value changed this frame. Read the value with element.text.

### `ui.clicked`

ui.clicked(element) — did it fire `clicked` THIS frame? The polling half of ui.on, for a manager that already has an update(). Reads the same event list the hooks fire from (published before scripts run), so a poll and a hook can never disagree.

### `ui.dragging`

ui.dragging() — the element being dragged, as a node, or nil. Live for the whole drag AND for the frame the `dropped` hooks run on. There is no separate payload channel because a node already carries params, a name and tags — ask it what it is.

### `ui.dropTarget`

ui.dropTarget() — the drop target the drag is currently over, as a node, or nil.

### `ui.event`

ui.event(element, "dropped") — did that element fire that hook this frame? Any hook by name; ui.clicked/pressed/released/changed/submitted are the shorthands.

### `ui.events`

ui.events() — everything that happened on the UI this frame, as { node = element, event = "clicked" } rows. ui.events("clicked") filters. Lets one manager handle a whole screen without naming a single element: for _, ev in ipairs(ui.events("clicked")) do ... end.

```lua
-- the whole screen, without naming a single element
function update(node, dt)
  for _, ev in ipairs(ui.events("clicked")) do
    log("clicked " .. ev.node.name)
  end
end
```

### `ui.focus`

ui.focus(node) — move the keyboard/gamepad focus. ui.focus(nil) drops it (a screen that wants nothing focused until the player touches something). Focusing a text field starts editing it.

### `ui.focused`

ui.focused() — the focused element as a node, or nil. ui.focused(el) answers yes/no for one element. Also readable per-node as node.focused.

### `ui.held`

ui.held() — the element the pointer is holding down, as a node, or nil. ui.held(el) answers yes/no. Hold-to-charge, press-and-hold repeat, a dip while pressed.

### `ui.hovered`

ui.hovered() — the element under the pointer, as a node, or nil. ui.hovered(el) answers yes/no for one element. A STATE, not an event: true for as long as it's true (hoverStart/hoverEnd are the edges).

```lua
-- a state, not an event: true for as long as it's true
local over = ui.hovered()
find("Caption").text = over and over.name or ""
```

### `ui.make`

ui.make(container, tree) — build a UI subtree from data and RECONCILE it with the one already there: call it again and only the difference is spawned and destroyed, so surviving rows keep their entity, their hover, their scroll and their in-flight transitions. An element is { "kind", prop = value, ..., children }, where kind is box/row/col/text/image/button/field/slider/scroll. `items = {...}` plus a function child makes one child per item (the function gets (item, i); return nil to skip it). `key = "id"` is how a row is matched through a re-sort. `onClicked = function(node) ... end` (any UI hook, `on` + its name) carries behaviour inline — no prefab, no script file. Properties the table stops mentioning go back to default; what the PLAYER did (scroll, typing, a toggle, a dragged slider) is kept. Play only, and a mistyped property raises rather than being ignored. Elements you placed by hand under the same container are never touched.

```lua
ui.make(find("Crew Panel"), {
  "col", gap = 8, pad = 12, style = "panel", items = crew,
  function(m) return { "text", key = m.id, text = m.name } end,
})
```

### `ui.off`

ui.off(element) stops every hook YOUR script is listening to on that element; ui.off(element, "clicked") stops one. Only your own — two managers on one button must not be able to unregister each other.

### `ui.on`

ui.on(element, "clicked", function(el, hook) ... end) — listen to an element from a script that does NOT live on it, so ONE manager holds a whole menu instead of a three-line script file per button. Any UI hook: clicked, pressed, released, hoverStart, hoverEnd, changed, submitted, cancelled, focusEnter, focusExit, dragStart/Move/Enter/Over/Leave/Cancel, dropped. The handler gets the element that fired and the hook name, so one function can serve a row of buttons. Registering again for the same element and hook REPLACES (so calling it from update() is harmless, not a leak). A listener dies with its element or with the script that registered it; a hot reload re-registers. Listening for an interaction the element doesn't take warns in the Console — it would otherwise be silent.

```lua
-- one menu script instead of a script file per button
function start(node)
  ui.on(find("Play"), "clicked", function() scene.load("level1") end)
  for _, b in ipairs(find("Toolbar"):children()) do
    ui.on(b, "clicked", function(el) selectTool(el.name) end)
  end
end
```

### `ui.pressed`

ui.pressed(element) — LMB went down on it this frame. Pair with ui.held(element) for hold-to-charge.

### `ui.released`

ui.released(element) — LMB came back up this frame (on or off the element).

### `ui.submitted`

ui.submitted(element) — Enter in this focused text field this frame.

### `ui.unbind`

ui.unbind(node) drops every binding on that node; ui.unbind(node, "text") drops one.

## networking — net.*, synced

### `net`

Multiplayer: host and join, synced state, RPCs, ownership (net.isMine), and the rollback readouts. Open netcode — you can self-host the relay.

### `net.despawn`

SERVER ONLY: net.despawn(node) — remove a replicated runtime object everywhere.

### `net.host`

net.host{ maxPlayers = 16, port = 7777, relay = "addr", interest = 150, interestBudget = 16384 } — become the authoritative host. relay = a rendezvous relay address (you get a LOBBY CODE, nobody port-forwards); port = direct UDP (QUIC) for LAN; neither = the in-editor loopback harness. interest = metres: each client hears about its own neighbourhood instead of the whole world (leave it off below a few dozen players — broadcasting is cheaper); interestBudget = bytes/sec of entity updates per client; inputDelay = rollback input delay in TICKS (clamped to 6) — omit it and the host derives one from the worst peer's measured RTT (2 on a LAN, 5 across a country).

### `net.inputDelay`

net.inputDelay() — the session's FIXED input delay in ticks. Never changes mid-match, because how the game feels must not.

### `net.isClient`

net.isClient() — true on a connected client.

### `net.isMine`

net.isMine(node) — is this node under MY control on this machine? Offline/non-networked → true; server → true unless a remote peer owns it; client → only your own predicted node(s). Cameras/HUDs use it to pick the local player out of many avatars (pair with findScripts).

### `net.isServer`

net.isServer() — true on the authoritative host.

### `net.join`

net.join(addr) — join a session: "relay://relayaddr/CODE" = a lobby code through a relay (no port-forwarding), "quic://host:port" = a server directly, "local://" = the in-editor test harness.

### `net.joinState`

net.joinState() -> state, reason — how a join is going: "offline" | "connecting" | "joined" | "refused". On "refused" the second return is the relay's own words ("no lobby QK7RM") — print it. WAIT ON THIS, not on net.role(): joining does not block, so role reads "client" from the frame you called net.join, whether or not that code matched any lobby.

### `net.leave`

net.leave() — end the session.

### `net.lobbyCode`

net.lobbyCode() — the code friends type in to join, on a host that used net.host{ relay = "…" }. Put it on your own lobby screen. nil until the relay answers (POLL it, don't read it once), and nil for good on a client or a direct/LAN host — there is no code there, joiners use the address.

### `net.mispredictRate`

net.mispredictRate() — 0..1, the fraction of simulated ticks that had to guess a peer's input. Rises with latency; what the input delay is chosen against.

### `net.on`

net.on(event, fn) — session events: playerJoined/playerLeft (peer id), connected, disconnected (reason).

### `net.peers`

net.peers() — connected client peer ids (server).

### `net.ping`

net.ping(peer?) — round-trip time in ms.

### `net.random`

net.random(a?, b?) — deterministic RNG for a rollback match, drawn from (match seed, tick, draw index): every peer rolls the same number AND a re-simulated tick rolls it again. Use this instead of rng() in anything a rollback node reads — an unseeded roll comes from the clock, and two peers drawing differently is a match that quietly forks in two. No args → [0,1); one → integer 1..a; two → a..b.

### `net.replaying`

net.replaying() — true while the engine is RE-SIMULATING ticks it already ran after a correction. For cosmetics the engine can't gate for you (a screen shake, a UI poke). NEVER branch simulation on it: a replayed tick that computes something different from the live one is the definition of a desync.

### `net.rewind`

SERVER ONLY, inside onRpc for an rpc sent {withInput=true}: run the closure against the world as that peer PERCEIVED it — raycasts and other scripts' synced vars read the rewound tick (clamped ~250 ms). A parry that was up on the attacker's screen counts.

### `net.role`

net.role() — "offline" | "server" | "client".

### `net.rollbackAverage`

net.rollbackAverage() — mean ticks re-simulated per correction. The texture of the connection, where rollbackMax is only its worst moment. A healthy match sits low.

### `net.rollbackDepth`

net.rollbackDepth() — ticks re-simulated by the most recent correction.

### `net.rollbackMax`

net.rollbackMax() — the deepest rollback this session has had to perform: its worst moment.

### `net.rpc`

net.rpc(name, args, {to=peer, withInput=true}) — remote call: server→clients or client→server. withInput stamps a client intent with the tick it was seeing (for net.rewind). Handle with function onRpc.name(args, sender). Args: scalars + tables (≤4 deep, ≤1KB).

### `net.setInputDelay`

net.setInputDelay(ticks) — the rollback input delay for the NEXT match, in ticks, clamped to 6. Too low and the opponent's input lands after the tick that needed it on every tick, so the driver guesses and re-simulates: correct, and five times the work. Fixed for a session on purpose — adaptive delay hides a bad connection by changing how the game FEELS while you are playing it. Call it between matches; the roster re-announce restarts the driver.

### `net.spawn`

SERVER ONLY: net.spawn(path, {x,y,z,owner}) — spawn a scene's first node as a replicated runtime object on every client (available next tick).

### `net.stalled`

net.stalled() — true while the sim is waiting for a peer's input rather than guessing past the depth cap. The game runs slightly slow instead of teleporting the opponent. Drive your own "connection trouble" banner off this — a stall is otherwise indistinguishable from a bad frame rate.

### `onRpc`

onRpc.<name>(args, sender) — handles net.rpc("name", args). sender is the verified peer id (0 = server).

### `replicated`

replicated = { hp = 100 } — declare synced script vars (top level). Read/write them as synced.hp; the server's writes replicate to every client.

### `restore`

function restore(s) — the other half of snapshot(): put the table back. Called before the engine re-simulates a tick it already ran. Restore every key snapshot() returned, and nothing else.

### `snapshot`

function snapshot() — REQUIRED on a rollback node's scripts. Return a flat table of every gameplay value this script owns (state, frame counters, health, stun). The engine calls it each tick and restores it when a correction arrives. ANYTHING you leave out is a value that survives a rewind unchanged — which is exactly what a desync is made of. Transforms and physics bodies are saved for you; do NOT put them in here.

### `synced`

The synced-vars table (declared via replicated = {...}). Server writes replicate; client writes warn and get overwritten.

## scenes — load, unload & persist

### `scene`

Which world is loaded: scene.load / scene.unload, additive layers, and scene.onLoaded. Pair with node.persistent to carry a node across a swap.

### `scene.current`

scene.current() — the running scene's name (its file stem, e.g. "first").

### `scene.list`

scene.list() — every scene in the project as names scene.load accepts (sorted; subfolders kept).

### `scene.load`

scene.load("arena") — switch to another scene at the next frame boundary: the world swaps, physics/animators/particles/audio rebuild, every start re-fires (like the scene booting fresh). Accepts a name, a scenes-relative path ("arenas/desert"), or "scenes/arena.ron". Multiplayer: only the SERVER may call it — every client follows automatically; a client's call is refused (send the server an RPC instead).

### `scene.onLoaded`

scene.onLoaded(function(name, additive) ... end) — run something once a scene has finished loading. Fires AFTER the world is whole, because a loading screen's whole job is to go away once the thing it was covering exists.

### `scene.unload`

scene.unload("Shop") — remove a scene that was loaded additively, and everything under it. The other half of scene.load{ additive = true }.

## terrain — runtime sculpt & queries

### `terrain`

Runtime sculpting and queries against the SDF terrain: dig, sculpt, paint, ask what is under a point, and persist edits per save slot.

### `terrain.deleteSaveDir`

terrain.deleteSaveDir("saves/slot2/terrain") — delete a save slot's persisted terrain from disk (pair with save.deleteSlot in a "delete this save" UI). Narrow by design: relative path, no "..", must not be the ACTIVE saveDir, and only .cfield/.tfield/.meta files in that one directory are removed (emptied dirs are tidied). Returns the number of files removed.

### `terrain.dig`

terrain.dig(x,y,z, radius [, strength]) — carve a hole: sugar for terrain.sculpt(..., "lower"). Pair with raycast(...) to dig where the player aims.

```lua
if input.clicked("left") then
  local hit = raycast(node.pos, node.forward, 6)
  if hit then terrain.dig(hit.x, hit.y, hit.z, 1.5) end
end
```

### `terrain.flush`

terrain.flush() — checkpoint every EDITED resident terrain field to the save slot (terrain.saveDir must be set). Runs IN THE BACKGROUND (amortized encode + threaded write, deferred while a field is actively being dug) so autosaves never stutter; exit paths (Stop / scene.load) finish the writes synchronously so a checkpoint is never lost.

### `terrain.generatePlanet`

terrain.generatePlanet(id [, opts]) — REPLACE terrain id's whole field with a generated planet (sphere ± noise relief, caves + chambers, molten core, craters, layered materials). Background-generated (seconds; Console shows progress). opts (all optional): radius, voxel, relief, bumpFreq, caveDepth, coreR, corePaint, craters, craterMin/Max, craterDust, surfaceA/B {slot,color}, patchBias/Thr, subsoil(+Depth), strata(+Depth), deep, pockets {slot,color,threshold,minDepth}, seam {slot,color,minDepth,center,width}, iceCaps {lat,slot,color}, seed.

### `terrain.height`

terrain.height(x, z) — world Y of the highest terrain surface under (x,z), or nil when nothing is hit. Spawning, footstep audio by ground, drop-to-floor.

### `terrain.paint`

terrain.paint(x,y,z, radius, r,g,b [, strength]) — recolor the terrain surface inside the brush ball (0..1 colors).

### `terrain.paintTexture`

terrain.paintTexture(x,y,z, radius, slot) — paint a terrain-palette texture slot (1-based, the Terrain tab's palette; 0 clears to flat color).

### `terrain.query`

terrain.query(x,y,z) — signed distance to the nearest terrain surface (negative = inside rock), or nil with no terrain. Cheap: read it every frame (burrow checks, depth meters).

### `terrain.saveDir`

terrain.saveDir(path) / terrain.saveDir() — set (or read) the game's SAVE-SLOT directory for player-edited terrain, relative to the project root (e.g. "saves/slot1/terrain"). While set, streaming loads fields from here first (before the project file or the genspec) and writes edited fields back on stream-out — per-slot terrain persistence. "" clears; auto-cleared when Play stops.

### `terrain.sculpt`

terrain.sculpt(x,y,z, radius [, strength [, mode]]) — sculpt the nearest terrain at a world point, landing the SAME tick (collision updates with the surface). mode: "raise" (default), "lower"/"dig", "smooth", "flatten"; strength 0..1. No-op when no terrain surface is near the point. Multiplayer: run on the server + mirror by RPC (deterministic ops).

### `terrain.slotAt`

terrain.slotAt(x, y, z) — the texture-palette slot at a world point, or nil where the field is untextured. The material half of the question terrain.query answers the distance half of: survey before you cut, and let a footstep know what it is standing on.

### `terrain.warm`

terrain.warm(bodyName) — keep that body's terrain RESIDENT this frame regardless of where the ship/player physically is: it streams in if cold and never streams out. Immediate mode — call every frame while you care (the map warms its focused planet). Streaming is otherwise anchored to dynamic bodies' physical positions, never the camera.

### `terrain.yields`

terrain.yields() — drains what recent digs actually removed: { id, removed, added, untextured, slots }, with slots mapping palette slot to volume. This is how mining pays out by MATERIAL — you get ore because you cut rock that was painted as ore.

## water — depth, buoyancy & ice

### `water`

Water volumes: how deep a point is (water.depthAt), what is in the water (water.at), freezing and thawing (water.setFrozen). The engine already does buoyancy and drag — these are the questions a GAME still has to answer: swimming, drowning, flooding, a gauge going red.

### `water.at`

water.at(point) — nil in air, else { depth, density, frozen, node, up }. `up` is the way OUT of the water (radial on a sea, the pool's own +Y) — what a swim controller pushes along, and NOT the same as -gravity in a tilted tank. Innermost volume wins, so a tank inside an ocean answers as the tank.

### `water.depthAt`

water.depthAt(x, y, z) — or a vec3, or a node. Metres BELOW the surface at that point; 0 in air. The one number everything else is derived from, and it is the same rule the solver uses, so a swim state can never disagree with the physics that floats you. A frozen volume reads 0 everywhere.

### `water.isUnderwater`

water.isUnderwater(point) — the yes/no, for when you don't need the depth. Takes x,y,z or a vec3 or a node: if water.isUnderwater(node) then stamina = stamina - dt end

### `water.setFrozen`

water.setFrozen(node, true) — freeze a water volume. Freezing is a STATE, not a second system: the same node with a flag flipped, and both the physics (no buoyancy, no drag) and the look follow from it. A world that thaws is one call back.

### `water.volumes`

water.volumes() — every body of water in the scene, as node handles. What a climate or weather system iterates when it wants to know where the seas are.

## scatter — instanced props

### `scatter`

Thousands of props from a seed — GPU-instanced, with no scene node anywhere in it. Your generator still decides WHAT grows where; the engine decides where each instance stands and draws them. scatter.create declares a source, scatter.remove harvests one.

### `scatter.cost`

scatter.cost(id) — what this source asks for every frame: { chunks, props, far, chunkSize, perChunk }. Read it BEFORE you ship the field. The knobs look like a look, but the outermost `lod` distance is really the budget: it sets how many chunks stay resident, as a sweep whose side grows with it, walked every frame. Cost is about (far/chunk)^2 per source — halving the distance, or doubling the chunk, quarters it. A field big enough to matter also says so in the Console when you declare it. On a body smaller than your view distance the count saturates at the body, so a planet never costs more than a planet.

### `scatter.create`

scatter.create{ asset = "tree.glb", seed = 7, perChunk = 24, chunk = 16 } — declare a source, get its id. Region: center + radius for a sphere (a planet), or center + halfX/halfZ for ground. Also scaleMin/scaleMax, align = "surface" (default) or "world", fade, and lod = { {asset=, distance=}, ... } nearest-first. Placement is a pure function of the seed, so every machine and every session grows the SAME forest without storing one. `density` is how a world gets biomes: pass a function(x, y, z) -> 0..1 and it is sampled ONCE, at declare time, into a densityRows grid (rows x 2*rows for a sphere's longitude) — 0 means no instance is generated at all, not a hidden one. An option this doesn't list is an error, not a shrug. `asset` may be a mesh file OR a .prefab.ron — a prefab is baked once into one instanced draw per Mesh node it holds, each at its authored place in the prop, which is how a prop your own script assembled gets scattered.

### `scatter.destroy`

scatter.destroy(id) — remove a whole source and everything it was drawing. Returns true if there was one.

### `scatter.near`

scatter.near(sourceId, point, radius) — the instances around a point, nearest first: { id, pos, distance, scale, param }. What a harvest verb aims with, and what a "is there room to build here" check reads.

### `scatter.remove`

scatter.remove(sourceId, instanceId) — take one prop out, permanently. By id rather than by position, which is what makes it survive streaming out and back in: an id comes from (seed, chunk, index), a position is a float off the end of a chain of arithmetic.

### `scatter.removed`

scatter.removed(sourceId) — the sorted ids this source has lost. A game that wants permanence saves THIS — a handful of numbers — not every plant it ever saw, which is what made permanence unstorable before (save values are capped at about a kilobyte).

### `scatter.restore`

scatter.restore(sourceId [, instanceId]) — put one prop back, or all of them when the instance is omitted (returns how many). This is what "the forest regrows after fifteen minutes" is, without your game having to remember what it cut.

## 2D — tilemaps & sprite batches

### `batch:draw`

b:draw(x, y [, z] [, scale] [, rot] [, cell] [, r, g, b, a]) — draw one sprite THIS FRAME, positioned in the batch node's local space. Immediate mode, exactly like draw.* : what you draw this frame is what shows, and next frame starts empty — there is no pool to grow and no clear() to forget. `scale` is one number, or a vec2 for squash-and-stretch: b:draw(x, y, 0, vec2(1.4, 0.6)). The tint is the thing a shared Material could never give one sprite: flash one enemy red without blinking it off.

### `node:setSpriteBatch`

node:setSpriteBatch{size=1.0} — make this node a SPRITE BATCH, so node:sprites() can draw into it. The counterpart of node:setTilemap: a game's sprite styles are data (one batch per material), so the nodes that draw them are made from the same script that declares them rather than authored one at a time into the scene. `size` is the quad's edge length; every sprite scales it. The sheet is the node's own Material.

### `node:setTilemap`

node:setTilemap{cols=13, rows=7, tile=1.5 [, data={…}]} — make this node a TILEMAP: a grid of spritesheet cells drawn as one mesh, one draw call. The sheet is the node's own Material (texture + sheetCols/sheetRows). Neighbouring tiles share an exact edge, so the hairline gaps a grid of separate quads opens up as the camera moves cannot happen. `data` is row-major from the top-left; leave it out for an empty grid you fill with tm:set.

### `node:sprites`

node:sprites() — a handle to this node's SpriteBatch (make it one with node:setSpriteBatch{} first; on any other node this is an error rather than a handle that silently draws nothing): b:draw(...) queues one sprite for this frame. N sprites from one node, each with its own position, rotation, scale, cell AND tint — no scene node per sprite and no pool to grow.

### `node:tilemap`

node:tilemap() — a handle to this node's tilemap grid: tm:set / tm:get / tm:fill / tm:size. Re-dress a room per floor without rebuilding the node.

### `tm:fill`

tm:fill(cell) — set every square, including the empty ones. The fast way to reset a room before re-dressing it.

### `tm:get`

tm:get(x, y) → cell, or nil outside the grid and on an empty square.

### `tm:set`

tm:set(x, y, cell) — set one square, 0-based from the TOP-LEFT. Outside the grid is a no-op rather than a wrap. Pass EMPTY_TILE (or use tm:fill) to clear.

### `tm:size`

tm:size() → cols, rows.

## vessels — assembly.*

### `assembly`

Multi-part vessels: hold forces and torques, split parts off, latch parts on, and read the compound's mass and centre of mass. A vessel is one physics body built from many nodes.

### `assembly.force`

assembly.force(node, force) — a HELD force through the centre of mass, re-applied every tick until you change it (engines, thrusters). Through the CoM means no torque: the vessel accelerates without turning.

### `assembly.forceAt`

assembly.forceAt(node, force, at) — a held world-space force at a world point. Off the centre of mass it produces torque as well as acceleration, which is how an off-axis thruster makes a craft tumble — and how RCS steers it.

### `assembly.impacts`

assembly.impacts(node) — the LAST tick's per-part contact loads: { part, impulse, speed, speedAbs, x, y, z }. What a damage model reads: how hard each part was hit and where.

### `assembly.impulseAt`

assembly.impulseAt(node, impulse, at) — a one-shot kick at a world point, applied once rather than held. Explosions, collisions you resolve yourself, a docking clamp letting go.

### `assembly.info`

assembly.info(node) — { mass, com, origin, vel, angVel, grounded, anchored, parts }. com is the world-space centre of mass as a vec3 — the number a flight controller, a CoM gizmo and a landing check all need.

### `assembly.keepLive`

assembly.keepLive(node, true) — exempt this compound from distant-craft LOD, so it keeps simulating in full even when nothing is near it. For the craft the player will come back to and expects to find where physics would have put it.

### `assembly.merge`

assembly.merge(node, other) — latch another assembly onto this one: docking, grabbing, a part snapping into place. The two become one physics body with one mass and one centre of mass.

### `assembly.rebuild`

assembly.rebuild(node) — re-gather the compound from the root's CURRENT children. Call it after you have added or removed part nodes yourself, so the physics body matches the scene again.

### `assembly.setAnchored`

assembly.setAnchored(node, true) — pin the vessel exactly where it stands (a launch clamp, a craft on a pad, anything that must not drift while you build it). Release it and normal physics resumes.

### `assembly.split`

assembly.split(node, parts [, fn] [, prefab]) — detach part nodes into their own assembly (stage separation, a wing coming off). The new assembly keeps the velocity it had, so debris carries on rather than appearing at rest.

### `assembly.syncColliders`

assembly.syncColliders(node) — re-pose the compound's collision shapes to its parts' current transforms. Needed after you move parts around without a rebuild, or the vessel collides with where it used to be.

### `assembly.teleport`

assembly.teleport(node, pos) — move the assembly origin to a world position, carrying every part with it. A teleport rather than a force: no acceleration, no tumble.

### `assembly.torque`

assembly.torque(node, t) — a held PURE torque, no linear push: reaction wheels, SAS, anything that turns a vessel without moving it.

## the camera & the screen

### `camera`

The game camera's projection: viewport size and rect, world↔screen conversion, and picking rays. camera.screenRect shares its space with input.mouse(), which is why hit-testing works.

### `camera.exists`

camera.exists() — true once a live game camera is being fed. Guard the other camera.* calls with it during the first frames, or while a scene without a camera is up.

### `camera.pixelsPerUnit`

camera.pixelsPerUnit([distance]) → px — how many screen pixels one world unit covers at that distance (default: the camera's distance from the origin). The number every 2D game used to derive by hand from the FOV and the camera's Z, and then snap the camera to a multiple of for crisp pixels.

### `camera.screenRect`

camera.screenRect() -> x, y, w, h — the game viewport in the SAME space as input.mouse() and camera.worldToScreen, offset included. That shared space is the only reason hit-testing the mouse against a projected point works; screenSize alone would be wrong wherever the viewport isn't at the window origin.

### `camera.screenSize`

camera.screenSize() → w, h — the game viewport size in pixels. camera.exists() is true once a live game camera is being fed.

### `camera.screenToRay`

camera.screenToRay(sx,sy) → ox,oy,oz, dx,dy,dz — a world ray from a screen pixel (inverse of worldToScreen).

### `camera.worldToScreen`

camera.worldToScreen(x,y,z) → sx, sy, depth, onscreen — project a world point into the game view (pixels in input.mouse()'s space). onscreen=false behind the camera / off-frustum. Sample a drawn line into points, project each, keep the nearest to the cursor = click-on-line picking (the map's maneuver nodes).

## physics controls — pause & step

### `physics`

Sim controls: physics.pause(true) freezes the whole gameplay tick while scripts keep running (pause menus, cutscenes, loading screens), and physics.step() advances it one tick at a time.

### `physics.isPaused`

physics.isPaused() — whether the sim is currently frozen, including when the editor froze it rather than your script.

### `physics.pause`

physics.pause(true) — freeze the whole gameplay tick while scripts keep running. Pause menus, cutscenes and loading screens are this call: the world stops, your UI doesn't.

### `physics.step`

physics.step([n]) — advance the frozen tick n times (default 1, max 600) — the same thing the editor's frame-step button does, so a game can build its own training mode. Call it from update: a fixedUpdate caller would never get a second turn, because the tick it is waiting for is the one it just stopped.

## persistence — save.*

### `save`

The persistent store: save.set / save.get, named slots, and flushing to disk. Values are capped at about a kilobyte each — store the small fact, not the whole world.

### `save.delete`

save.delete("gold") — remove a key; true if something was removed.

### `save.deleteSlot`

save.deleteSlot("slot2") — delete a slot's store file from disk ("delete this save" UIs). Deleting the ACTIVE slot also empties the in-memory store, so the slot is instantly reusable. Per-slot terrain is separate — pair with terrain.deleteSaveDir. Returns true if a file was removed.

### `save.flush`

save.flush() — write the store to disk NOW (checkpoints, before risky sections). Returns false on an IO error (also shown in the Console).

### `save.get`

save.get("gold" [, default]) — the stored value, else the default, else nil. save.get("who").hp reads into stored tables.

### `save.set`

save.set("gold", 42) — store persistent game data: survives Play sessions, editor restarts, and ships with exported builds. Values follow the synced-var guardrails (numbers/strings/bools/tables, depth <= 4, <= 1 KB). Flushed on Stop + every few seconds during Play.

```lua
save.set("hp", hp)                 -- survives scene loads and quits
hp = save.get("hp", 100)
```

### `save.slot`

save.slot("slot2") — switch the active save slot (the old one flushes first); save.slot() reads the current name. Each slot is its own file under save/.

## timers — after, every, tween

### `after`

after(seconds, fn) — run fn once after that much GAME time (tick-driven, deterministic, pauses with the game). Returns a handle: h:cancel() aborts. Capture what you need as locals — the callback gets no arguments. after(2, function() door.visible = false end)

```lua
after(0.25, function() spawnEffect("Explosion", node.pos) end)
```

### `every`

every(seconds, fn) — run fn repeatedly (first fire after one period). Anchored cadence: long sessions don't drift. Keep the handle to stop it: local h = every(1, tickDown) ... h:cancel().

```lua
-- a heartbeat that survives long sessions without drifting
every(1.0, function() hp = math.min(hp + 1, 100) end)
```

### `timer:cancel`

timer:cancel() — stop a pending after / every / tween. The handle those three return exists for exactly this: local h = every(1, tick) ... h:cancel().

### `tween`

tween(seconds, fn [, ease]) — animate: fn(alpha) runs every tick with alpha easing 0→1, final call exactly at 1.0. ease: "linear" (default), "smooth", "in", "out". tween(0.5, function(a) node.y = startY + a * 3 end, "smooth"). Returns a cancellable handle.

```lua
-- SECONDS first, then the function; alpha eases 0 -> 1 and lands on 1.0
tween(0.4, function(t) node:getcomponent("UiElement").opacity = t end, "smooth")
```

## space — orbits & time-warp

### `body.mu`

Gravitational parameter µ = GM.

### `body.name`

The celestial body's node name — what space.body() takes and space.dominant() returns.

### `body.radius`

Physical surface radius.

### `body.soi`

Sphere-of-influence radius (-1 = infinite, the root).

### `body.vx`

World velocity X — the body's own motion along its rails, which a rendezvous has to match.

### `body.vy`

World velocity Y.

### `body.vz`

World velocity Z.

### `body.x`

World X of the body's centre this tick.

### `body.y`

World Y of the body's centre.

### `body.z`

World Z of the body's centre.

### `space`

On-rails celestial mechanics: where the bodies are, which one's gravity owns a point, the orbit a craft is on, and time-warp.

### `space.bodies`

space.bodies() — every celestial body this tick: {name, x,y,z, vx,vy,vz, mu, radius, soi} in world coords (soi -1 = infinite). space.body("Pebble") grabs one by node name.

### `space.body`

space.body("Pebble") — one celestial body by node name: { name, x,y,z, vx,vy,vz, mu, radius, soi } in world coordinates, or nil. space.bodies() returns them all.

### `space.dominant`

space.dominant(x, y, z) — the name of the body whose gravity OWNS that position (deepest sphere of influence — the moon inside the planet inside the sun), or nil.

### `space.elements`

space.elements(x,y,z, vx,vy,vz) — the orbit a craft is ON around its dominant body: { body, a, e, periapsis, apoapsis, period } (apoapsis/period absent on an escape). Distances from the body CENTER. The map/HUD readout.

### `space.gravity`

space.gravity(x, y, z) — gx, gy, gz: the µ/r² pull of the dominant body at a world position (patched conics: exactly one body pulls).

### `space.propagate`

space.propagate(px,py,pz, vx,vy,vz, mu, dt) — the state (px,py,pz, vx,vy,vz) advanced dt seconds on the two-body conic about a point mass mu (elliptic OR hyperbolic, drift-free). The map's maneuver nodes + SOI-encounter walk are built from it. State is in whatever frame you pass — compose parent frames yourself.

### `space.time`

space.time() — on-rails celestial time in seconds (0 at Play start; advances with warp). Scenes with Celestial Body components put planets/moons on exact Kepler rails.

### `space.warp`

space.warp(50) — request a time-warp multiplier (1 .. 100000): rails fast-forward, local physics keeps ticking at 1×. space.warp() reads the current value.

## components — getcomponent

### `cam.active`

The play-mode view camera (1/0) — assign true to switch to it.

### `cam.fovY`

Vertical field of view, radians.

### `light.b`

Color blue 0..1.

### `light.g`

Color green 0..1.

### `light.intensity`

Brightness multiplier.

### `light.r`

Color red 0..1.

### `light.range`

Reach in world units.

### `mat.cell`

Which cell of the sheet draws (row-major from the top-left; clamped into the grid).

### `mat.sheetCols`

Sheet columns (0 = not a sheet — the whole texture).

### `mat.sheetRows`

Sheet rows.

### `node:getcomponent`

node:getcomponent(name) — a component handle whose fields you can read AND assign at runtime (applies live during play), or nil if absent. Components: RigidBody (friction, restitution, gravity, kinematic 1/0 — live Dynamic/Kinematic switch, shape 0/1/2, radius, height, half_x/y/z, lock_x/y/z, lock_rot_x/y/z), PointLight (intensity, range, r/g/b), Camera (fovY radians, active — assign true to switch cameras), ParticleSystem (play_on_start), UiElement (visible, opacity, posX/posY, width/height, radius, border, fillRGBA, textSize, textRGBA, tintRGBA, cell — spritesheet frame), UiSlider (value/min/max — drive a health bar), UiLayer (enabled, z, designHeight, worldSpace). e.g. node:getcomponent("RigidBody").friction = 0.02 for ice.

```lua
local rb = node:getcomponent("RigidBody")
if rb then rb.friction = on_ice and 0.02 or 0.6 end
```

### `rb.friction`

Surface friction 0..1 (0 = frictionless).

### `rb.gravity`

Gravity pull on this body (1/0; assign true/false).

### `rb.half_x`

Box half-extent X.

### `rb.half_y`

Box half-extent Y.

### `rb.half_z`

Box half-extent Z.

### `rb.height`

Capsule total height.

### `rb.kinematic`

Transform-driven mode (1/0; assign true/false, live): never falls or gets pushed, but PUSHES dynamic bodies — platforms, elevators, grabbed objects. (Static mode is the Inspector dropdown — a baked collider, nothing to toggle here.)

### `rb.lock_rot_x`

Freeze rotation about X (1/0).

### `rb.lock_rot_y`

Freeze rotation about Y (1/0).

### `rb.lock_rot_z`

Freeze rotation about Z (1/0).

### `rb.lock_x`

Freeze world X translation (1/0).

### `rb.lock_y`

Freeze world Y translation (1/0).

### `rb.lock_z`

Freeze world Z translation (1/0).

### `rb.radius`

Sphere/capsule radius.

### `rb.restitution`

Bounciness 0..1 (0 = no bounce).

### `rb.shape`

Body shape: 0 = sphere, 1 = capsule, 2 = box.

## animation — node:animator

### `anim:clips`

anim:clips() — every playable state name, as a list.

### `anim:crossfade`

anim:crossfade("Idle", 0.3 [, layer]) — transition with an explicit fade time (seconds).

### `anim:current`

anim:current([layer]) — alias of anim:state: the state currently showing (topmost active layer). Nil when idle.

### `anim:duration`

anim:duration("Punch") — the clip's AUTHORED length in seconds (nil if there's no such state). Reads the asset, not playback, so it works in start().

### `anim:events`

anim:events("Punch") — the clip's authored events as { {t = seconds, func = "onHitboxStart"}, … }, ascending by t; nil if there's no such state, an empty list if it has none. Reads the asset, so you can bake integer frame data at load: frame = math.floor(e.t / anim:duration(c) * totalFrames + 0.5). Prefer this to letting events DRIVE gameplay — they fire off float playback time, quantise to sample_fps, and are deliberately not re-fired on a prediction replay.

### `anim:finished`

anim:finished([layer]) — true when a non-looped state reached its end this frame (or stays true while holding the last frame).

### `anim:isPlaying`

anim:isPlaying([state]) — is that state playing on any layer (or anything at all, with no argument)?

### `anim:layers`

anim:layers() — every layer name, base first, as a list.

### `anim:play`

anim:play("Run" [, fade [, layer]]) — transition to a state. The controller supplies the crossfade (default fade, per-arrow overrides, and a state's ⇥ fade-in override which beats everything — 0 = instant); pass `fade` to override the first two. Safe to call every frame — re-playing the current state is a no-op.

### `anim:restart`

anim:restart("Attack" [, fade [, layer]]) — like play, but re-enters even if that state is already playing (re-trigger a one-shot).

### `anim:seek`

anim:seek(t [, layer]) — jump the current state's playhead to t seconds.

### `anim:setLayerWeight`

anim:setLayerWeight("Attack", 0.5) — blend a layer over the ones below (0 = off, 1 = full override).

### `anim:setSpeed`

anim:setSpeed(2) — global playback speed multiplier for this node's animator.

### `anim:state`

anim:state([layer]) — the state currently showing (topmost active layer), or that layer's state. Nil when idle.

### `anim:stop`

anim:stop([layer [, fade]]) — stop a layer (all layers if omitted). Higher layers release to the layers below; the base returns to its default state.

### `anim:time`

anim:time([layer]) — seconds into the current state.

## particles — effects from script

### `node:particles`

node:particles() — the particle handle for this node's Particle System component. Setters: :play/:stop/:restart/:setIntensity/:setBeamEnd. Getters: :isPlaying/:alive/:asset. e.g. on a hit, node:particles():restart() to re-fire a burst.

### `particles:alive`

particles:alive() — live particle count across the effect's tracks (0 when stopped).

### `particles:asset`

particles:asset() — the effect asset key this node's Particle System references, or nil.

### `particles:isPlaying`

particles:isPlaying() — true while an instance is emitting/ageing on this node.

### `particles:play`

particles:play() — start emitting if the effect is idle (spawns a fresh instance). No-op if already playing.

### `particles:restart`

particles:restart() — re-spawn from t=0 (re-fire a one-shot burst, e.g. a muzzle flash on each shot).

### `particles:setBeamEnd`

particles:setBeamEnd(x, y, z) — aim every Beam track's endpoint at a WORLD-space point (the engine converts it to effect-local, so the beam keeps tracking the target as the node moves). Re-call per tick to follow a moving target.

### `particles:setIntensity`

particles:setIntensity(i) — live emission scale (0..~2): multiplies rates/burst counts and shades particle size. Drive an engine plume off the throttle without touching the asset.

### `particles:stop`

particles:stop() — stop + despawn the effect; its live particles vanish.

### `spawnEffect`

spawnEffect(key, x, y, z) — fire a one-shot particle effect at a world point, no node needed. It plays once and despawns itself. e.g. local h = raycast(...); if h then spawnEffect("vfx/Impact", h.x, h.y, h.z) end.

## audio — sounds & the mixer

### `audio`

Sounds and the mixer: audio.play for one-shots, audio.track for a mixer bus, node:sound() for a node's Audio Source.

### `audio.play`

audio.play(clip [, node | x, y, z] [, opts]) — play a clip with no setup: audio.play("audio/ding.ogg") is flat 2D; pass x,y,z for a world point; pass a node to follow it. opts: {volume, pitch, pan, mode="Spatial|Distance|Flat", falloff="Inverse|Linear|Exponential", minDistance, maxDistance, track, endBehavior="Stop|Destroy|Loop", loop=true}. Returns a sound handle: :stop/:pause/:resume/:setVolume/:setPitch/:setPan/:setTrack/:setPosition/:seek/:isPlaying/:position. e.g. audio.play("audio/hit.ogg", h.x, h.y, h.z, { maxDistance = 35, track = "SFX" })

```lua
audio.play("audio/footstep", node, { track = "SFX", volume = 0.6, minDistance = 4 })
```

### `audio.stopAll`

audio.stopAll() — stop every playing sound (sources and one-shots), with a click-free fade.

### `audio.track`

audio.track(name) — a live mixer-track handle ("Master" or a track from the Mixer tab): :setVolume(db), :setPan(-1..1), :setMuted(bool), :setSoloed(bool). Changes affect the running session only and revert on Stop. e.g. audio.track("Music"):setVolume(-12) to duck music.

### `sound:isPlaying`

Still audible (false once finished)?

### `sound:pause`

Freeze playback.

### `sound:position`

Playhead in seconds.

### `sound:resume`

Continue a paused sound.

### `sound:seek`

Jump the playhead to a time in seconds.

### `sound:setPan`

Stereo pan −1..1 (non-spatial sounds).

### `sound:setPitch`

Playback-rate pitch (0.5 = octave down, 2 = octave up).

### `sound:setPosition`

Move the emitter (stops following a node).

### `sound:setTrack`

Re-route through a mixer track (\"Master\" or a track name).

### `sound:setVolume`

Linear volume (1 = as authored).

### `sound:stop`

Fade the sound out and end it.

### `source:isPlaying`

Is the source audible right now?

### `source:pause`

Freeze playback (resume continues from here).

### `source:play`

Play the source's clip from the start (restarts if already playing).

### `source:position`

Playhead in seconds.

### `source:resume`

Continue a paused sound.

### `source:seek`

Jump the playhead to a time in seconds.

### `source:setClip`

Swap the clip (project-relative path like \"audio/steps.ogg\"); restarts playback if playing.

### `source:stop`

Fade the sound out (a few ms — no click).

### `track:setMuted`

Mute / unmute the track.

### `track:setPan`

Stereo pan −1..1.

### `track:setSoloed`

Solo the track (mutes everything else).

### `track:setVolume`

Fader gain in dB (0 = unity, −60 = silent).

## assets

### `assets`

Reference files under Assets/ in code: assets.getFile(path), assets.getContents(dir).

### `assets.getContents`

assets.getContents("models") — an array of every file under that folder (recursive). Build tables of assets with it.

### `assets.getFile`

assets.getFile("models/armor.glb") — the asset's path (or nil), to hand to node.model / node.material. Path is relative to Assets/.

## debug gizmos

### `gizmo`

Immediate-mode debug drawing (play mode): gizmo.line/ray/sphere/point show for ONE frame in the Scene view (never the Game view; the viewport gizmos toggle hides them). Call every frame you want a shape visible.

### `gizmo.line`

gizmo.line(x1,y1,z1, x2,y2,z2 [, r,g,b]) — a world-space debug line for one frame. Color is 0–1 floats (default green).

### `gizmo.point`

gizmo.point(x,y,z [, size [, r,g,b]]) — a small 3-axis cross marking a spot: hit points, waypoints, spawn locations.

### `gizmo.ray`

gizmo.ray(ox,oy,oz, dx,dy,dz [, len [, r,g,b]]) — a debug ray: origin + direction. With `len` the direction is normalized and the ray is that long — mirrors raycast(...), perfect for visualizing ground checks / line-of-sight.

### `gizmo.sphere`

gizmo.sphere(x,y,z [, radius [, r,g,b]]) — a wire debug sphere (three rings): trigger zones, blast radii, pickup ranges.

## lua stdlib

### `math.abs`

math.abs(x) — absolute value.

### `math.approach`

math.approach(current, target, maxDelta) — move toward target without ever overshooting. Pass `rate * dt`; this is the correct version of the hand-rolled move-towards that jitters at low frame rates.

```lua
-- frame-rate correct, never overshoots
throttle = math.approach(throttle, target, params.rate * dt)
```

### `math.approachAngle`

math.approachAngle(current, target, maxDelta) — math.approach for headings: turns the short way and never overshoots. Turrets, camera yaw, 'face the player'.

### `math.clamp`

math.clamp(x, lo, hi) — x held inside the range. Reversed bounds are tolerated rather than returning NaN.

```lua
hp = math.clamp(hp + heal, 0, 100)
```

### `math.cos`

math.cos(x) — cosine of x (radians).

### `math.deg`

math.deg(rad) — radians to degrees.

### `math.deltaAngle`

math.deltaAngle(a, b) — the SHORTEST signed turn from a to b, correct across the +/-pi seam (350 degrees to 10 is +20, not -340).

```lua
-- the short way round, across the +/-pi seam
local turn = math.deltaAngle(node.yaw, wanted)
node.yaw = math.approachAngle(node.yaw, wanted, params.turn_rate * dt)
```

### `math.fbm`

math.fbm(x, y, z [, octaves [, seed]]) — seeded fractal noise (default 4 octaves, rotated so features never align to the axes), about -1..1. Terrain-style variation for scripts: scatter decorations, vary spawns, wobble paths.

### `math.floor`

math.floor(x) — round down.

### `math.inverseLerp`

math.inverseLerp(a, b, x) — where x sits between a and b, 0..1. Returns 0 when a == b instead of a NaN that poisons everything downstream.

### `math.lerp`

math.lerp(a, b, t) — linear blend, UNCLAMPED (t outside 0..1 extrapolates, which is useful). Use math.mix for the clamped version.

### `math.max`

math.max(a, b, …) — largest argument.

### `math.min`

math.min(a, b, …) — smallest argument.

### `math.mix`

math.mix(a, b, t) — math.lerp with t clamped to 0..1.

### `math.noise`

math.noise(x, y, z [, seed]) — seeded value noise, one octave, about -1..1, identical on every machine (the same numbers the engine's Rust generators use). Scale the inputs to pick a frequency.

### `math.pi`

The constant π.

### `math.pingPong`

math.pingPong(t, len) — 0 to len and back, forever. Patrols, bobbing, breathing lights.

### `math.rad`

math.rad(deg) — degrees to radians.

### `math.random`

math.random() — random in [0,1); math.random(n) — 1..n.

### `math.remap`

math.remap(x, a, b, c, d) — x from the range a..b onto c..d. The one-liner behind fades, falloffs and gauge needles.

```lua
local alpha = math.remap(distance(node, player), 5, 25, 1, 0)
```

### `math.round`

math.round(x [, step]) — nearest whole number, or nearest multiple of `step`: `math.round(x, 0.25)` snaps to quarters for grid placement.

### `math.saturate`

math.saturate(x) — clamp to 0..1, the most-written clamp of all.

### `math.sign`

math.sign(x) — -1, 0 or 1. Exactly 0 for 0 (not 1, which is what math.abs tricks give you).

### `math.sin`

math.sin(x) — sine of x (radians).

### `math.smoothstep`

math.smoothstep(a, b, x) — 0..1 with eased ends, for anything that shouldn't start and stop abruptly.

### `math.sqrt`

math.sqrt(x) — square root.

### `math.wrapAngle`

math.wrapAngle(a) — an angle folded into (-pi, pi].

### `rng`

rng(seed) — a DETERMINISTIC random stream: same seed, same sequence, every machine. r:next() in [0,1), r:range(a,b), r:int(a,b) inclusive, r:pick(list). Use for gameplay that must reproduce (loot, procgen scatter, server replays); math.random stays for throwaway rolls.

### `rng:int`

Uniform integer in [a, b] inclusive.

### `rng:next`

Uniform in [0, 1).

### `rng:pick`

A uniform element of `list` (nil if empty).

### `rng:range`

Uniform in [a, b).

### `string.format`

string.format(fmt, …) — printf-style formatting.

### `table.copy`

table.copy(t) — a shallow copy (keys and values).

### `table.count`

table.count(t [, fn]) — how many entries (works on KEYED tables, which `#t` cannot), or how many satisfy the predicate.

### `table.extend`

table.extend(dst, src) — append src's items onto dst in place, and return dst.

### `table.filter`

table.filter(list, fn) — a new list of the items where fn(value, i) is true.

```lua
local ready = table.filter(ships, function(s) return s.fuel > 0 end)
```

### `table.find`

table.find(list, fn) -> value, index — the first item satisfying the PREDICATE (nil, nil if none). `table.find(ships, function(s) return s.docked end)`.

```lua
local docked, i = table.find(ships, function(s) return s.docked end)
```

### `table.indexOf`

table.indexOf(list, value) — the index of a value by plain equality, or nil.

### `table.keys`

table.keys(t) — the keys as a SORTED list. Sorted because raw `pairs` order is hash order, which a replay can't reproduce.

### `table.map`

table.map(list, fn) — a new list of fn(value, i). Never mutates the input.

```lua
local names = table.map(crew, function(m) return m.name end)
```

### `table.reverse`

table.reverse(list) — a new list, back to front.

### `table.sum`

table.sum(list [, fn]) — add the numbers, or add fn(value, i) over them: `table.sum(tanks, function(t) return t.fuel end)`.

