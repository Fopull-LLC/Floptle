# Scripting in Floptle (Lua)

Game logic in Floptle is written in **Lua**. A script is a `.lua` file in your
project's `scripts/` folder. Attach it to a node and it runs every frame while the
game is playing. Scripts **hot-reload** — save the file and the running game picks
it up immediately.

> The same reference is built into the editor: open the **Scripting** tab → **§ Docs**,
> and the API shows up as autocomplete + hover hints as you type.

**This page teaches; [`lua-api.md`](lua-api.md) lists.** Read this one in order to learn
how the pieces fit, and go to the reference when you already know the name and want the
signature — it has every call the engine exposes, grouped and searchable, and it is
generated from the same table the editor's Docs tab reads, so the two never disagree.

## Contents
1. [A first script](#1-a-first-script)
2. [Lifecycle: `start`, `update`, `fixedUpdate`](#2-lifecycle-start-update-fixedupdate)
3. [`node` — the transform](#3-node--the-transform)
4. [`node` — the physics body](#4-node--the-physics-body)
5. [`input` — keyboard & mouse](#5-input--keyboard--mouse)
6. [Globals: `params`, `time`, `dt`, `log`](#6-globals-params-time-dt-log)
7. [Assets & swapping models / materials](#7-assets--swapping-models--materials)
8. [Referencing other nodes & scripts](#8-referencing-other-nodes--scripts)
9. [Animation: `node:animator()`](#9-animation-nodeanimator)
10. [Particles: `node:particles()`](#10-particles-nodeparticles)
11. [Audio: `audio.play`, `node:sound()` & the mixer](#11-audio-audioplay-nodesound--the-mixer)
12. [Recipe: a walkable first-person character](#12-recipe-a-walkable-first-person-character)
13. [Bundled example scripts](#13-bundled-example-scripts)
14. [The in-engine IDE](#14-the-in-engine-ide)
15. [Tips & gotchas](#15-tips--gotchas)
16. [Networking: `net.*`, `synced`, `onRpc`](#16-networking-net-synced-onrpc)
16b. [Rollback netcode: `snapshot`, `restore` & `net.random`](#16b-rollback-netcode-snapshot-restore--netrandom)
17. [Scenes: `scene.load` & the entry scene](#17-scenes-sceneload--the-entry-scene)
18. [Layers & tags](#18-layers--tags)
19. [Vectors & math: `vec3`, `vec2`, `distance`](#19-vectors--math-vec3-vec2-distance)
20. [Collision & trigger events](#20-collision--trigger-events)
21. [Prefabs: `spawn` & `destroy`](#21-prefabs-spawn--destroy)
22. [Terrain: `terrain.sculpt`, `dig` & queries](#22-terrain-terrainsculpt-dig--queries)
22a. [Water: volumes, buoyancy & `water.*`](#22a-water-volumes-buoyancy--water)
22b. [Scatter: thousands of props from a seed](#22b-scatter-thousands-of-props-from-a-seed)
23. [Saving: `save.set`, `save.get` & slots](#23-saving-saveset-saveget--slots)
24. [The web: `http.*` & `json.*`](#24-the-web-http--json) — full page: [web-api.md](web-api.md)

---

## 1. A first script

```lua
-- spin.lua — slowly rotate the node it's attached to.
defaults = { speed = 45 }            -- tunables (also editable in the Inspector)

function update(node, dt)
  node.yaw = node.yaw + math.rad(params.speed) * dt
end
```

Attach it by dragging the `.lua` from **Assets** onto a node, dropping it on the
Inspector's **Scripting** section, or **Inspector → Scripting → + Add Script**.
Press **F1** to Play.

Compound assignment operators work: `+=  -=  *=  /=  %=  ^=  ..=`.

```lua
node.yaw += math.rad(params.speed) * dt
```

## 2. Lifecycle: `start`, `update`, `fixedUpdate`

```lua
function start(node)             -- optional; runs once when Play begins
end

function update(node, dt)        -- runs every frame while playing
end

function fixedUpdate(node, dt)   -- runs every GAMEPLAY TICK (60 Hz, constant dt)
end

function lateUpdate(node, dt)    -- runs after physics each frame (the camera pass)
end
```

Each attached script keeps its **own state across frames** — assign a variable in
`start` (or at the top level) and read it back in `update`.

**Which one do I use?** The split is simple:

| Hook | Cadence | Put here |
|---|---|---|
| `update` | every rendered frame (variable `dt`) | cosmetic motion, UI-ish logic |
| `fixedUpdate` | every gameplay tick (constant `dt`, 60 Hz) | movement, gameplay rules, velocity/physics writes |
| `lateUpdate` | every rendered frame, AFTER physics | **cameras & followers** — anything that tracks another node |

**Why `lateUpdate` for cameras:** the engine's frame order is scripts →
animation → physics → *interpolated transform writeback* → `lateUpdate`. A
camera positioned in `update` reads its target's pose from **before** this
frame's physics — one frame stale, a follow error of `velocity × dt` that
turns frame-time noise into visible movement jitter. In `lateUpdate` the
target's pose is final for the frame, so the follow is exact. The stock
`third_person_camera.lua` does this.

`fixedUpdate` runs on the same fixed clock physics steps on, right before each
physics tick — so gameplay code behaves identically at 30 fps and 240 fps, and
`input.pressed(...)` edges are delivered **per tick** there (a press between two
ticks is never lost). It's also the cadence multiplayer prediction will replay,
so code you put in `fixedUpdate` today is already netcode-shaped.

> Inside `fixedUpdate`, the `input` API reads the tick's input snapshot; inside
> `update`, the frame's. Both work everywhere — only the timing window differs.

## 3. `node` — the transform

`node` is synced from the node's transform *before* each call and read back *after*,
so setting a field moves the object.

| Field | Meaning |
|---|---|
| `node.x` `node.y` `node.z` | Position, world units |
| `node.yaw` `node.pitch` `node.roll` | Rotation, **radians** (YXZ order) |
| `node.scale` | Uniform scale (shortcut for all axes) |
| `node.scale_x` `node.scale_y` `node.scale_z` | Per-axis scale |

### 3.1 Directions & orientation

Pointing at things used to be the one corner of the API you had to write out
longhand — `atan2` with two minus signs, a four-line project-onto-plane. Each of
these names the intent instead, and none of them can get the sign wrong.

| Call | What it does |
|---|---|
| `node:lookAt(target [, up])` | Face a node or a world point. Sets yaw + pitch; with an `up`, the roll too |
| `node:turnTowards(target, maxRadians)` | Turn toward it by at most that much — the short way round. Pass `rate * dt` |
| `dirTo(from, to)` | The unit direction between two things (nodes, points, anything with x/y/z) |
| `yawOf(dir)` / `pitchOf(dir)` | The angles that face along a direction |
| `dirFromYaw(yaw [, pitch])` | …and back again: the direction those angles face |
| `lookRotation(dir [, up])` | → `yaw, pitch, roll`, without applying them |

```lua
function update(node, dt)
  local enemy = find("Enemy")
  -- Snap to face it…
  node:lookAt(enemy)
  -- …or swing round at 3 rad/s, which is what a turret actually wants.
  node:turnTowards(enemy, 3 * dt)

  -- Fire along the way you're facing:
  local hit = raycast(node.pos, dirTo(node, enemy), 50)
end
```

`turnTowards` takes a **node** (or a world point) as somewhere to face, and any
other vector as a **direction** — so `node:turnTowards(node.vel, 6 * dt)` steers a
unit to face where it is going.

> Nothing here produces a NaN. A zero-length direction leaves the facing alone,
> `yawOf(vec3(0,0,0))` is `0`, and `dirTo(p, p)` is `vec3(0,0,0)`.

### 3.2 On the ground, on any planet — `:flatten(up)`

"Forward, but along the ground" is a projection onto the plane perpendicular to
up. On a flat world that is "drop the Y"; on a planet, up is radial and changes
as you walk. One method covers both:

```lua
local up = node.up or vec3(0, 1, 0)   -- -gravity: Y on a flat world, radial on a planet
local fwd = dirFromYaw(node.yaw):flatten(up)
local right = fwd:cross(up)           -- already in the plane, already unit length

node.vel = (fwd * forwardInput + right * strafeInput) * speed + up * node.vel:dot(up)
```

That is the whole of `first_person.lua`'s movement basis, and it runs unchanged
on a planet. `:flatten()` with no argument uses +Y.

### 3.3 Local ↔ world

`node.x/y/z` are **local** — measured from the parent. Handy for moving something,
wrong for comparing it against a world target (see
[§8](#where-is-it-really--nodeworldxworldyworldz)). The full set:

| Call | Meaning |
|---|---|
| `node.worldX/worldY/worldZ`, `node.worldPos` | Where it really is (read-only) |
| `node:setWorldPos(v)` | Put it at a world point, whatever it's parented to |
| `node:toWorld(v)` / `node:toLocal(v)` | A point through this node's own frame |
| `node:worldForward()` / `worldRight()` / `worldUp()` | Its axes after the parent chain |
| `node:distanceTo(other)` | Distance in **world** space, to a node or a point |
| `node:distanceFlat(other [, up])` | …ignoring the up axis (default +Y) |

```lua
-- Where is the muzzle? The gun is parented to an arm that is parented to a
-- character — toWorld composes all of it, including scale.
local muzzle = gun:toWorld(vec3(0, 0, -1.2))
spawn("Bullet", muzzle, function(b) b.vel = gun:worldForward() * 60 end)
```

`node.forward` is the node's **local** forward. A gun barrel on a swinging arm
points where the *arm* says, so shooting along `node.forward` misses — that is
what `worldForward()` is for.

### 3.4 Getting there — movement & easing

| Call | Meaning |
|---|---|
| `node:moveTowards(target, maxDelta)` | Walk toward a world point without overshooting. Returns `true` on arrival |
| `moveTowards(node, target, maxDelta)` | The same thing, spelled as a free function |
| `ease(a, b, rate, dt)` | Frame-rate-independent exponential ease. Numbers **or** vectors |
| `smoothDamp(cur, target, vel, smoothTime, dt)` | → `value, vel` — a critically-damped spring, with momentum |
| `v:towards(other, maxDelta)` | The vector version of `math.approach` |

```lua
-- A patrol, in two lines.
function update(node, dt)
  if node:moveTowards(waypoints[i], params.speed * dt) then
    i = i % #waypoints + 1
  end
end

-- A camera follow that feels the same at 30 fps and at 240.
function lateUpdate(node, dt)
  node.pos = ease(node.pos, target.pos + offset, params.smoothing, dt)
end
```

`ease` moves a *fraction of what's left* each second, so it never quite arrives
and never overshoots — that is what makes it frame-rate independent, and why
three shipped camera scripts each defined it privately before it lived here.
`smoothDamp` is the one to reach for when the follow should keep moving for a
moment after the target stops.

## 4. `node` — the physics body

These extra fields appear **only when the node has a Rigidbody** (Inspector →
**◆ Rigidbody**). Instead of teleporting the node, you drive its **velocity** and the
engine integrates it (gravity, collisions, ground contact).

| Field | R/W | Meaning |
|---|---|---|
| `node.vx` `node.vy` `node.vz` | read/write | Velocity (m/s). Read the current value, modify it, write it back. |
| `node.grounded` | read | `true` while the body rests on a surface. Gate jumps on it. |
| `node.up_x` `node.up_y` `node.up_z` | read | The body's **up** = −gravity. `[0,1,0]` on a flat world, **radial** on a planet. |
| `node.height` | read/write | Capsule standing height. Write a smaller value to **crouch** (feet stay planted). |
| `node.groundNormal` | read | The floor it stands on, as a vec3 — `nil` while airborne. |
| `node.wallNormal` | read | The steepest surface it is **pressed against**, as a vec3 — `nil` when there's only floor. |

The golden rule for movement: **keep the velocity's vertical (gravity/jump) part,
replace the horizontal part.**

```lua
local vy = node.vy
if node.grounded and input.pressed("space") then vy = params.jump end
node.vx = move_x
node.vz = move_z
node.vy = vy
```

Because `node.up_*` is the surface normal of gravity, a controller that moves along
it and jumps along it works on **flat worlds and on spherical planets** with no extra
code (see the character recipe below).

### Slopes: don't push into what you can't walk up

A walking controller that drives straight into a steep face **launches itself**.
Nothing is bouncing it: the solver resolves the overlap by pushing the capsule
out along the surface normal, that normal points partly *upward*, and a
controller that keeps pushing collects that push again every single frame. At a
run into a 70° hillside it is tens of metres per second of free climb.

The two normals are the fix. Take the into-the-surface part out of your movement
and what remains is a slide along it:

```lua
-- cos of the steepest ground you allow: 50° here
local steep = math.cos(math.rad(params.slope_limit))

local function slide(m, n)          -- m = desired velocity (vec3), n = a normal or nil
  if not n or n:dot(node.up) >= steep then return m end   -- nothing there, or walkable
  local into = m:dot(n)
  if into >= 0 then return m end                          -- already moving away
  return m - n * into                                     -- slide along the face
end

local move = slide(slide(move, node.wallNormal), node.groundNormal)
```

`wallNormal` is the cliff you ran at; `groundNormal` catches a slope you are
standing on that is still ground but steeper than you want to allow. The shipped
`first_person.lua` and `third_person.lua` both do exactly this —
`params.slope_limit` in the Inspector.

One more line pays for itself: while grounded and **not** jumping, drop any
upward velocity you didn't ask for. It came from being pushed out of a slope or
a step, and keeping it is how a walk turns into a takeoff.

```lua
if node.grounded and not jumping and vup > 0 then vup = 0 end
```

The body's **tunables** — friction, bounciness, gravity on/off, shape/size, axis
locks — are scriptable too, via `node:getcomponent("RigidBody")` (see
[§7](#7-assets--swapping-models--materials)).

### Body modes: Dynamic, Kinematic, Static

The Rigidbody's **mode** dropdown replaces hand-freezing axes and disabling
gravity:

| Mode | What it does | Cost |
|---|---|---|
| **Dynamic** | Fully simulated: gravity, velocity, collisions push it around. | normal |
| **Kinematic** | **Transform-driven**: never falls or gets pushed — your scripts/animation move the node and the body follows. Dynamic bodies collide **with** it (a moving platform *carries and pushes* the player), raycasts hit it, touch events fire. | near zero |
| **Static** | **Baked collider** in the body's shape — no body at all. The cheapest way to make something solid (walls, floors, props). | zero per tick |

```lua
-- a moving platform: Kinematic mode + plain transform writes
defaults = { dz = 6.0, speed = 0.5 }
local from
function start(node) from = node.pos end
function update(node, dt)
  local t = (math.sin(time * params.speed * math.pi * 2) + 1) * 0.5
  node.pos = from:lerp(from + vec3(0, 0, params.dz), t)
end
```

Scripts can flip **Dynamic ↔ Kinematic live** (grab an object, dock a vehicle):

```lua
node:getcomponent("RigidBody").kinematic = true   -- freeze + carry it
node:getcomponent("RigidBody").kinematic = false  -- drop it (wakes at rest)
```

Every mode can also be a **trigger** (the Rigidbody's trigger checkbox): the
body becomes a sensor that never blocks anything but fires the
`onTriggerEnter/Stay/Exit` hooks on overlap — Kinematic + trigger is the
moving pickup / sweeping damage zone (see
[§20 Triggers](#20-collision--trigger-events)).

Static is authoring-time (it's a collider, not a body — switch it in the
Inspector; the live sim rebuilds instantly). All three modes ride the scene
format, so replicated/spawned nodes behave identically over the network — a
server-moved Kinematic platform replicates its transform like any node, and
clients keep its collision hull where the players *see* it.

### 4.1 Assemblies: `assembly.*` — multi-part vessels

Tick **assembly** on a Dynamic RigidBody and that node roots ONE compound
6-DOF rigid body built from every descendant node that carries a RigidBody:
each part becomes an oriented shape at its offset, weighted by its `mass`
field (the root's own shape fields are ignored). Composed mass, center of
mass and inertia are real — thrust that doesn't point through the CoM
*torques the vessel*, and landing on one leg tips it. Ships, rovers, cranes,
breakable structures.

```lua
-- fixedUpdate: hold thrust for this tick (world space; re-arm every tick —
-- a dropped call means the engine stops, nothing latches).
function fixedUpdate(node, dt)
  local i = assembly.info(node)         -- mass, com, vel, angVel (vec3 tables),
  if i == nil then return end           -- grounded, parts (entity ids)
  local up = vec3(0, 1, 0)
  if input.down("space") then
    -- 20 kN straight up THROUGH an engine mounted at the base: if that point
    -- is off the CoM line, the vessel honestly starts to rotate.
    assembly.forceAt(node, vec3(0, 20000, 0), vec3(i.com.x + 0.4, i.com.y - 2, i.com.z))
  end
  assembly.torque(node, vec3(0, 0, 400))          -- reaction-wheel roll
end

-- Staging: detach parts into a NEW live vessel (a fresh root node; the part
-- nodes re-parent under it, physics momentum hands off exactly).
assembly.split(node, { boosterA, boosterB }, function(stage)
  assembly.impulseAt(stage, vec3(0, -800, 0), assembly.info(stage).com)  -- sep spring
end)
```

**Splitting off something FLYABLE:** pass a prefab name as the fourth argument
and the detached half is rooted at a fresh instance of it — scripts and all —
instead of a bare node. That's the difference between shedding debris and
undocking a lander that can fly home:

```lua
assembly.split(node, moduleParts, function(craft)
  save.set("handoff." .. craft.id, subBlueprint)   -- its controller reads this in start()
end, "Vessel")                                     -- prefab root: needs an assembly RigidBody
```

**Docking, cranes, construction — `assembly.merge(node, other)`:** the inverse
of `split`. Two compounds become ONE rigid body carrying their combined
momentum, `other`'s part nodes re-parent under this root with their world pose
kept, and `other`'s root is retired. The join is perfectly **inelastic**: an
off-centre catch spins the pair up exactly as much as it should, and whatever
relative motion is left at the instant of the latch is felt as a jolt — so aim
for a slow, aligned closing (or add your own magnetism to make one). Absorbed
parts keep their entity ids, so per-part contact attribution
(`assembly.impacts`) carries straight across the join. Latching onto something
`setAnchored`'d pins the pair.

```lua
-- A docking latch, in full: line the two up, then weld.
if range < 0.5 and align > 0.8 and closingSpeed < 1.5 then
  assembly.merge(myVessel, theirVessel)   -- next tick, their parts are my children
end
```

`assembly.force(node, f)` pushes through the CoM; `assembly.impulseAt` is a
one-shot kick (explosions, docking bumps). All vectors are world-space
`vec3`s. Forces are **held per tick** and applied through every physics
substep — call them from `fixedUpdate` for continuous thrust.

**Script-assembled vessels:** spawn part prefabs as children of an assembly
root (`spawn(part, pos, fn, vesselNode)`), then call
`assembly.rebuild(vesselNode)` once — the compound re-gathers from the
root's current descendants. That's the whole blueprint-spawner pattern.

**Anchoring (launch clamps, latches, cranes):** `assembly.setAnchored(node,
true)` pins the vessel exactly where it stands — no gravity, no contacts,
velocities read zero, held forces are ignored — and it still rides a moving
celestial's frame. `assembly.setAnchored(node, false)` releases it *from
rest* (nothing banks up while clamped). `assembly.info(node).anchored`
reports the state; a `rebuild` preserves it.

**Staying live when the camera roams:** distant compounds drop out of full
physics into a cheap LOD (landed ones freeze, in-flight ones coast on analytic
Kepler rails) and wake on approach — great for hundreds of deployed craft, but
a craft you're flying from a far-off view (e.g. a map camera pulled hundreds of
metres back) would freeze under you. `assembly.keepLive(node, true)` exempts a
compound from that LOD so it stays in full physics — live throttle, steering
and orbital velocity — however far the camera is; `assembly.keepLive(node,
false)` rejoins the LOD.

**Placing a live assembly:** the compound writeback owns the root node's
transform, so plain `node.x = …` writes are overwritten every frame —
`assembly.teleport(node, pos)` is THE way to move one (velocity untouched):
pad pinning, save restores, cutscene placement.

**Assembly roots read like bodies:** `node.vx/vy/vz`, `node.up_x/y/z`
(local gravity-up) and `node.grounded` all work on an assembly root, so
cameras and controllers written for single rigidbodies follow a vessel
unchanged.

**Surface structures on orbiting worlds:** a `Static`-bodied node parented
(at any depth) under a celestial body's node rides its orbit — the transform
hierarchy carries the visuals and the engine carries the baked collider. A
launchpad parented to its planet stays exactly as solid as the terrain.

**Distant craft cost nothing:** compounds far from the camera (~700 units)
leave live physics automatically — landed or slow ones freeze in their
planet's carried frame, in-flight ones coast on analytic Kepler rails
(drift-free at any warp) — and wake to full contact physics on approach.
Deploy hundreds of satellites, stages and rovers; only the neighborhood
simulates. While parked this way `info.anchored` reads true.

**Pausing physics wholesale:** `physics.pause(true)` skips the entire
physics step each tick while scripts, rails and terrain streaming keep
running — the tool for loading screens, cutscenes and pause menus
(`physics.pause(false)` resumes; `physics.isPaused()` reads it; queued
thrust is dropped, never banked, while paused).

**Frame-stepping:** `physics.step([n])` freezes the whole gameplay tick and
releases exactly `n` (default 1), each advancing scripts, physics *and*
animation by one frame — enough to build a training mode's own frame stepper.
The editor does the same thing from its ⏭ Step button (F3, while paused), with
the tick counter beside it naming the frame you stopped on. Call `physics.step`
from `update`: the frame pass still runs while the tick is frozen, `fixedUpdate`
by definition does not.

**Per-part impact attribution — `assembly.impacts(node)`:** the engine
attributes every contact a compound resolves to the PART that took it. Each
tick the call returns an array of `{ part, impulse, speed, x, y, z }` —
`part` is the part node's entity id (match `child.id` over `node:children()`
or `info.parts`), `impulse` the total normal impulse that part absorbed this
tick (mass·Δv), `speed` the peak closing speed it hit at this tick (m/s),
`x/y/z` its hardest contact point in world space. Empty between contacts;
anchored assemblies make no contacts at all. Poll it from `fixedUpdate` and
compare against per-part strength — that is a damage model in ten lines:

```lua
for _, hit in ipairs(assembly.impacts(node)) do
  if hit.speed > crashToleranceOf(hit.part) then   -- m/s, KSP-style
    spawnEffect("Explosion", hit.x, hit.y, hit.z)
    -- shear the part off as wreckage:
    assembly.split(node, { childById(node, hit.part) }, function(junk) end)
  end
end
```

Prefer `speed` over `impulse` for a crash test: the contact solver's
depenetration is BUDGETED (so a deep or fast spawn un-buries at a sane rate
instead of catapulting), which spreads a high-speed crash's impulse over many
ticks — the per-tick `impulse` plateaus and understates the hit. `speed` is
the pre-resolution normal closing velocity and is NOT capped, so it faithfully
reports how hard something struck (and it needs no mass normalization: a
40-tonne ship and a 4-tonne ship judge the same touchdown the same way). A
soft landing on legs reads as a low `speed` on the leg parts; a nose-first
lithobrake reads as a high `speed` on the nose. The solar demo's vessels break
exactly this way (`solar/scripts/vessel_controller.lua`).

### 4.2 Two telegraph layers: `draw.*` (game) vs `gizmo.*` (debug)

They look similar but serve different masters:

- **`draw.*` is part of your GAME** — always rendered in the game view, no
  editor toggle involved. Attach-point markers, selection outlines, range
  rings, orbit conics: player-facing linework. Immediate mode (re-issue
  every frame/tick you want it visible), world space, alpha supported.
  - `draw.line(x1,y1,z1, x2,y2,z2, r,g,b [,a])`
  - `draw.ring(cx,cy,cz, nx,ny,nz, radius, r,g,b [,a])` — a circle around
    the normal `n`
  - `draw.sphere(cx,cy,cz, radius, r,g,b [,a])` — three rings
  - `draw.box(cx,cy,cz, hx,hy,hz, yaw, r,g,b [,a])` — wireframe box
  - **Filled** primitives (solid triangles, for polished gizmos & markers):
    - `draw.tri(x1,y1,z1, x2,y2,z2, x3,y3,z3, r,g,b [,a])` — one triangle
    - `draw.cone(bx,by,bz, dx,dy,dz, radius, height, r,g,b [,a])` — a solid
      cone (base at `b`, apex `height` along unit dir `d`): arrowheads,
      nozzles, markers
    - `draw.disc(cx,cy,cz, nx,ny,nz, r0, r1, r,g,b [,a])` — a filled annulus
      (inner `r0`, outer `r1`) around normal `n`; `r0=0` is a full disc.
      Rotation-gizmo bands, ring markers
- **`gizmo.*` is for DEBUGGING** (`gizmo.line/ray/sphere/point`) — drawn
  only while the editor's viewport gizmos toggle (and its Script filter)
  is on, exactly like collider/light overlays. Ground-check rays, AI
  targets, physics probes: developer eyes only, never the player's.

## 5. `input` — keyboard & mouse

Available while playing.

| Call | Returns |
|---|---|
| `input.key("w")` | `true` while the key is held |
| `input.pressed("space")` | `true` only on the frame it goes **down** (an edge) |
| `input.released("space")` | `true` only on the frame it goes **up** (an edge) |
| `input.typed()` | the **characters** entered this frame, as a string (see below) |
| `input.axis("a", "d")` | `-1` / `0` / `1` from a negative/positive key pair |
| `input.button(1)` | mouse button held (`0` left, `1` right, `2` middle) |
| `input.clicked(1)` | mouse button pressed this frame (an edge) |
| `local dx, dy = input.mouse_delta()` | mouse movement since last frame |
| `local x, y = input.mouse()` | cursor position, pixels |
| `input.scroll()` | wheel delta this frame |
| `input.setMouseLocked(true)` | pin + hide the cursor (FPS mouselook); `false` releases. Also `input.lockMouse()` / `input.unlockMouse()` |

### Getting the cursor back for your own menus

Clicking into the Game view pins the pointer there, so playing doesn't let the
mouse wander onto editor panels. The editor skips that if your game already has
something clickable on screen — a menu's buttons *are* the gameplay, and pinning
the pointer froze them dead.

That question is asked **every frame**, so a shop or a pause menu that opens two
minutes into a session takes the pointer back on its own; you don't have to do
anything, and the player doesn't have to know that Escape was an option.

`input.setMouseLocked(false)` also releases it explicitly, which is what to
reach for if your menu is drawn some way the editor can't recognise as
interactive (`draw.*`, a shader, your own hit-testing).

> Before **0.26.0** the question was only asked at the click that pinned the
> pointer. A game with no visible buttons during play — a twin-stick shooter,
> anything cursor-free — could never get the cursor back, and `setMouseLocked`
> did not release it either, because that is a separate lock owner. Its own
> menus were unclickable for the rest of the session.

Key names are **the same names the action map's key picker shows** (Project
Settings ⏵ Input) — one list, so a key you can bind is a key you can poll:

- `a`–`z`, `0`–`9`, `f1`–`f12`
- `space` `enter` `escape` `tab` `backspace` `delete` `insert` `home` `end`
  `pageup` `pagedown`
- `shift` `ctrl` `alt` `super` `capslock` (left and right collapse onto one name)
- arrows `left` `right` `up` `down`
- `,` `.` `/` `;` `'` `` ` `` `[` `]` `\` `-` `=`
- numpad `num0`–`num9` `num+` `num-` `num*` `num/` `num.`

> Before **0.21.1** the raw-key half of that list stopped at the arrows, so
> `input.pressed("f9")` — and every numpad, bracket and navigation key — was
> permanently `false` while the *same* key bound fine in Settings. If you worked
> around it, the workaround is no longer needed.

#### Which keys reach the game

Every key in that list, with **three exceptions the editor keeps for itself**:

| key | what takes it |
|---|---|
| `f1` | Play / Stop |
| `f2` | Pause |
| `f3` | Step one tick (Shift+F3 steps back) |

Those are the transport controls, and a game that could take `f1` could stop you
stopping it — which is the one key you need when a script has gone wrong. Polling
one of them writes a Console line the first time, and the Scripting tab lints it
where you wrote it, so it is never a mystery.

Everything else is the game's, **`tab` included**. That is worth stating because
until **0.33.0** it was not: egui gave Tab to the editor's own focus traversal
before the game saw it, so a press cycled the editor's panels and
`input.pressed("tab")` returned `false` (`floptle/0084`). Tab is *the* convention
for opening an inventory — Minecraft, Terraria, Valheim, Don't Starve — so it is
the first key both a player and a developer reach for, and the failure had no
symptom: `false` is exactly what a key nobody pressed looks like. A game shipped a
bag on Tab, passed its headless tests, and heard about it from a player. It also
worked in an exported build, so the binding looked broken for the whole time you
were making the game and correct only after you stopped testing it. A focused Game
view now claims the keyboard in the editor and in a build alike.

A locked cursor is genuinely pinned to the window center (hardware lock where
the OS supports it, per-frame re-centering where it doesn't) — read motion with
`input.mouse_delta()`. Stop always releases the lock.

**`input.pressed` is a key; `input.typed` is a character.** `input.pressed("q")`
asks about the *physical* key where Q sits on a QWERTY board — on AZERTY that
key types `a`, and nothing in the name says so. `input.typed()` returns what
the player meant to write, resolved by the OS layout, with a paste (Ctrl/Cmd-V)
folded into the same string. It never contains control characters: Enter and
Backspace stay actions.

```lua
code = code .. input.typed()
if input.pressed("backspace") then code = code:sub(1, -2) end
```

Building a string by polling `a`–`z` gets the alphabet wrong for anyone whose
keyboard isn't yours, and gets it wrong for digits and punctuation on every
keyboard. For anything more than a few characters, use a **UI text field** —
it brings a caret, selection, the clipboard and key repeat with it
([ui-navigation.md](ui-navigation.md)). `input.typed()` is empty while a field
has focus, because the field consumed them.

### Gamepads a script can actually see

Every other call here answers the *resolved* question — did this player press
Jump. None of them can answer the one underneath it: **is there a controller
here at all**. So "the pad was never enumerated", "it went into a slot the map
doesn't bind", "the window hasn't got focus" and "something downstream ate it"
all reach you as the same observation — nothing happens — and the bug report
that comes back is "controllers don't work".

| Call | Returns |
|---|---|
| `input.pads()` | a list of `{ index, name, connected }`, index 1-based |
| `input.padCount()` | how many pads are connected right now |
| `input.padButton(1, "South")` | that pad's button, **raw** — no action binding involved |
| `input.padAxis(1, "LeftStickX")` | that pad's axis, raw, −1..1 (triggers 0..1) |

```lua
for _, p in ipairs(input.pads()) do
  log(string.format("pad %d: %s%s", p.index, p.name, p.connected and "" or " (gone)"))
end
```

If the list is empty it is a *device* problem and nothing about your input map
matters. If the pad is listed but `input.action(...)` stays false, the pad is
fine and the **binding** is where to look — and `input.padButton` will tell you
the button is physically down while the action is not firing. Put that on a
controls screen and a player can diagnose it for you.

The list follows hot-plug: poll it, and a disconnected pad reads neutral rather
than freezing its last pose. Button and axis names are the variant names
(`South`, `East`, `LeftBumper`, `Start`, `LeftStickX`, `RightZ`, …), matched
case-insensitively; an unknown name reads `false`/`0` rather than erroring.

### Two local players on one axis

A binding can be scoped to a local player slot, which is what lets one `Move`
axis carry WASD for player 1 and the arrow keys for player 2 — and one pad per
player:

```ron
Stick( player: Some(0), id: Slot(0), x: LeftStickX, y: LeftStickY, deadzone: 0.25 ),
Stick( player: Some(1), id: Slot(1), x: LeftStickX, y: LeftStickY, deadzone: 0.25 ),
```

`player` is available on every binding form — actions, `Keys`, `Stick` and
`Analog`. **Without it two pads do not mean two players**: `Slot(n)` names a
*device*, not a player, so an unscoped pair contributes both sticks to both
players and the harder push drives both characters.

A player-scoped `id: Any` means *that player's own pad, or nothing* — so a
second player with no pad reads zero instead of mirroring the first player's
stick. Unscoped `Any` keeps its old meaning: the resolving player's pad, else
the first connected one.

### The camera projection (`camera.*`)

Turn a world point into a screen pixel (and back) against the **active game
camera** — the pixels are in the same space `input.mouse()` reports, so you can
hover and click 3-D things you drew:

| call | returns |
| --- | --- |
| `camera.worldToScreen(x, y, z)` | `sx, sy, depth, onscreen` |
| `camera.screenToRay(sx, sy)` | `ox,oy,oz, dx,dy,dz` (a world ray from a pixel) |
| `camera.screenSize()` | `w, h` (game viewport, pixels) |
| `camera.screenRect()` | `x, y, w, h` — the viewport's rectangle in cursor space. Use this, not `screenSize`, to ask "is the cursor over the game view?": in the editor the view is a docked panel, so `input.mouse()` carries its offset. (An edge-pan camera that compares the cursor's x against the *width* slides away forever the moment the panel is not at x = 0.) |
| `camera.exists()` | `true` once a live game camera is being fed |

`onscreen` is `false` for points behind the camera or outside the frustum — skip
those. **Click-on-line picking** (how the solar map's maneuver nodes are placed):
sample a drawn line into points, `worldToScreen` each, and keep the nearest to
`input.mouse()` within a pixel threshold; create at that point on `input.clicked(0)`.

```lua
local mx, my = input.mouse()
local best, bd
for _, p in ipairs(orbit_points) do
  local sx, sy, _, on = camera.worldToScreen(p.x, p.y, p.z)
  if on then
    local d = (sx - mx) ^ 2 + (sy - my) ^ 2
    if not bd or d < bd then best, bd = p, d end
  end
end
if best and bd < 18 * 18 and input.clicked(0) then create_node_at(best) end
```

### Raycasting

`raycast(ox,oy,oz, dx,dy,dz, max [, ignore])` casts a ray against the world's
colliders (the terrain **and** any walkable mesh colliders) **and every physics
body** (players, crates) and returns a hit table or `nil`:

```lua
-- ground within 1.2 units below me?
local h = raycast(node.x, node.y, node.z, 0, -1, 0, 1.2)
if h then
  -- h.x, h.y, h.z   the hit point
  -- h.nx, h.ny, h.nz the surface normal there
  -- h.distance       how far the ray travelled
  -- h.node           the node whose BODY was hit (nil for static geometry)
end
```

When the ray hits a body, `h.node` tells you whose: `h.node:getscript("combat")`
reaches its scripts. Your own node's body never blocks your rays, and the
optional `ignore` arg skips one more node's body — the orbit camera passes the
character it follows, so it never reads as a wall.

The last argument can instead be an **options table**, which also filters by
[layer](#18-layers--tags):

```lua
-- only the ground can block this ray — other players/props never will
local h = raycast(x, y, z, 0, -1, 0, 2.0, { ignore = target, layers = { "Ground" } })
```

`layers` takes one name or an array (Project Settings → Layers) and filters
**both** static geometry and bodies; a misspelled layer name is an error, not a
silent miss.

Use it for ground checks, line-of-sight, shooting, or dropping objects onto a surface.
(The built-in `node.grounded` already does a robust contact check for the character;
raycast is the general-purpose tool for everything else.)

### Shape queries — `overlapSphere`, `spherecast`, `capsulecast`

A ray answers *what is along this line*. A melee swing, an explosion or a
"can I fit there" asks a different question — *what is inside this volume* — and
a fan of rays answers it badly: it misses anything thinner than the fan and
cannot tell you how deep the overlap was.

```lua
-- Everything within 2 m of the sword, deepest overlap first.
for _, hit in ipairs(overlapSphere(swordTip, 2.0, { layers = "Enemies" })) do
  combat.hurt(hit.node, 25)
end

-- A thrown rock: a swept sphere hits what a ray squeaks past.
local h = spherecast(node.pos, vel:normalized(), 0.4, 30, { layers = {"Ground","Props"} })

-- "Can I actually walk there", asked with the shape that will be walking.
local blocked = capsulecast(node.pos, moveDir, 0.4, 0.9, 1.5)
```

| call | result |
|---|---|
| `overlapSphere(center, radius [, opts])` | a **list** of hits, deepest overlap first (empty when nothing is inside) |
| `spherecast(origin, dir, radius, max [, opts])` | the first hit, or `nil` |
| `capsulecast(origin, dir, radius, halfHeight, max [, opts])` | the first hit, or `nil` |

Hits carry the same fields a `raycast` hit does — `x/y/z`, `nx/ny/nz`,
`distance` and `node` — so a script that handles one handles the others. For an
overlap, `distance` is the **penetration depth** rather than a travel distance.
`opts` is the same table `raycast` takes, and your own body is skipped for you.

These are cheap here for a structural reason: every collider already answers a
signed distance, so an overlap is one distance test and a swept sphere is the
ray march with the radius subtracted. Unlike a ray, they also see **sensors** —
a hitbox usually does want to know it swept a trigger volume.

### Debug gizmos

Draw one-frame debug shapes over the viewport straight from code. They show in
the **Scene view only** (the Game view stays clean — it's what the player would
see), and the viewport's gizmos toggle hides them all. Colors are optional
`0–1` floats (default green); everything is **immediate mode** — call it every
frame you want the shape visible.

| Call | Draws |
|---|---|
| `gizmo.line(x1,y1,z1, x2,y2,z2 [, r,g,b])` | a world-space line |
| `gizmo.ray(ox,oy,oz, dx,dy,dz [, len [, r,g,b]])` | origin + direction (with `len` the direction is normalized — mirrors `raycast`) |
| `gizmo.sphere(x,y,z [, radius [, r,g,b]])` | a wire sphere (trigger zones, blast radii) |
| `gizmo.point(x,y,z [, size [, r,g,b]])` | a small 3-axis cross (hit points, waypoints) |

```lua
-- visualize a ground probe: green when it hits, red when it misses
local h = raycast(node.x, node.y, node.z, 0, -1, 0, 1.5)
if h then
  gizmo.ray(node.x, node.y, node.z, 0, -1, 0, 1.5, 0.3, 1.0, 0.4)
  gizmo.point(h.x, h.y, h.z, 0.2)
else
  gizmo.ray(node.x, node.y, node.z, 0, -1, 0, 1.5, 1.0, 0.35, 0.3)
end
```

The bundled character controllers ship with exactly this: set their `debug_ray`
param to `1` in the Inspector and the ground-check probe draws itself.

## 6. Globals: `params`, `time`, `dt`, `log`

| Global | Meaning |
|---|---|
| `params` | This instance's tunables — a table **seeded from `defaults`**, so `params.speed` works out of the box. The Inspector overrides individual values per node. |
| `time` | Seconds since Play started |
| `dt` | Seconds since the last frame (also the 2nd arg to `update`) |
| `log("…")` | Print to the engine **Console** |
| `print(anything, …)` | Console print that understands the whole engine: tables render **deeply** (nested, sorted keys, short arrays inline, cycle-safe), node handles print as `node "Player" (#4) at vec3(…)`, component/script handles by what they point at, vectors via their components. Multi-line output folds into a collapsible block in the Console. |

The full Lua standard library (`math`, `string`, `table`, …) is available.

> **`defaults` → `params`:** every key you put in `defaults` is readable as
> `params.<key>`. Declaring `defaults` is what makes a value tweakable per-node in the
> Inspector; if you don't override it there, `params.<key>` is just the default.

### String params

A **plain string default** becomes an Inspector **text field** on each instance
— so two portals share one script but carry different destinations:

```lua
-- portal.lua
defaults = { destination = "hub" }   -- each portal's Inspector shows a text box

function onTriggerEnter(node, other, hit)
  if other:hasTag("player") then scene.load(params.destination) end
end
```

Numbers and strings follow the same rules (seeding, live Inspector sync, the
two-way behavior below). A string that *looks like* `noderef()` output is a
reference param, not a string — those keep their picker.

### Describing your tunables to the Inspector

`defaults` says *what* your tunables are. `--@` comments say how they should be
**presented** — and the Inspector then draws a designed panel instead of a stack of
anonymous drag values, in **declaration order**:

```lua
defaults = {
  --@header Movement
  -- How fast you walk on flat ground.        <- a plain comment is the tooltip
  --@range 0 20 --@units m/s
  walk = 4.5,

  --@desc Blend between the walk and run animations.
  --@slider 0 1 --@step 0.05
  blend = 0.35,

  --@header Assist
  --@options Off|On|Auto
  assist = 1,               -- a NUMBER + options → dropdown, value = the index
  --@options walk|run|sprint
  gait = "walk",            -- a STRING + options → dropdown of those strings
  invert = false,           -- a boolean default → a checkbox, no annotation needed
  --@color
  tint = "#ff8800",         -- a swatch; the script still reads the hex string
  --@hidden
  debugScale = 1.0,         -- kept out of the Inspector entirely
}
```

| Annotation | Effect |
|---|---|
| `--@header Text` | A section rule above this row (underscores render as spaces). |
| `--@desc Text` | The row's tooltip. Repeat the line to build a paragraph. |
| *(a plain comment above the key)* | Used as the tooltip when there's no `--@desc` — so scripts that already document their tunables get hover text for free. |
| `--@range min max` | Clamps the value and bounds the drag. |
| `--@slider min max` | Draws a slider instead of a drag value. |
| `--@step n` | Drag speed / slider granularity. |
| `--@units m/s` | Suffix shown after the number. |
| `--@options a\|b\|c` | A dropdown. On a **string** param the value is the label; on a **number** it's the index (0, 1, 2 …). |
| `--@color` | A colour swatch over a `#rrggbb` string param. |
| `--@multiline` | A text box instead of a single-line field. |
| `--@hidden` | Don't show this tunable at all. |
| `--@about Text` | Describes the **script** (write it above `defaults`). |
| `--@editorButton Label fn` | A button that runs `fn(node)` in **edit** mode. |

They're comments: nothing changes at runtime, deleting them breaks nothing, a
misspelled one is ignored rather than fatal, and several can share a line.

**Booleans are real booleans.** A `flag = false` default round-trips as a boolean,
so `if params.flag then` means what it says — it's carried as 0/1 between the
Inspector and the script, and converted back on the way in (every number is truthy
in Lua, so a leaked `0` would have been permanently `true`).

### `params` is two-way

Writing a declared tunable **persists** — the next frame reads your value back,
the Inspector shows it update **live** during Play, and other scripts see it
through a handle. Stop reverts it with the rest of the play session. So state
you'd otherwise keep in a `local` can live in `params` when you want it visible
and tweakable:

```lua
defaults = { distance = 6.0 }

function lateUpdate(node, dt)
  params.distance = params.distance - input.scroll()   -- sticks, shows live
end
```

- Only **declared** keys persist (present in `defaults`, or already stored on
  the node). Assigning an undeclared key works for the current frame but is
  not saved — declare it if you want it kept.
- Reference params (`noderef()` & friends) never round-trip — they stay wired
  by the Inspector.
- Inspector edits during Play flow the other way instantly, so you can tune a
  value the script is also reading. If the script *writes* the same key every
  frame, its write wins — write only when changing (like the scroll above).

## 7. Assets & swapping models / materials

Scripts can reach into the project's **`Assets/`** folder and change a node's
components at runtime — swap a mesh's model, apply a material — so one script can drive
a whole wardrobe of looks.

### `assets` — referencing files in code

`assets` resolves files by a path written **relative to `Assets/`** (the same path the
Asset Browser shows; right-click any asset ▸ **Copy asset path** to grab it).

| Call | Returns |
|---|---|
| `assets.getFile("models/armor.glb")` | the asset's path (a string you hand to `node.model` / `node.material`), or `nil` if it doesn't exist |
| `assets.getContents("models")` | an array of **every file** under that directory (recursive) — great for building tables |

```lua
-- Build a database of armor models once, then swap between them.
local armor = {
  assets.getFile("models/armor/leather.glb"),
  assets.getFile("models/armor/iron.glb"),
  assets.getFile("models/armor/gold.glb"),
}
-- …or grab a whole folder at once:
local allTextures = assets.getContents("textures")
```

### `node.model` — swap a mesh's model

On a **Mesh** node, `node.model` reads its current model path and **writing it swaps the
model live** (the engine re-imports and renders the new one):

```lua
function update(node, dt)
  if input.pressed("e") then
    node.model = assets.getFile("models/armor/gold.glb")   -- equip gold
  end
end
```

### `node.material` — apply a material

Assign a **material preset** (by name, or an `assets.getFile("materials/…ron")`) and the
node takes on that look:

```lua
node.material = "Gold"                              -- a preset by name
node.material = assets.getFile("materials/Rusty.ron")
```

### `node.visible` — show / hide geometry

Toggle whether a node's mesh/shape is drawn (it keeps its transform, physics, and
children — only the visual is hidden). Also a checkbox in the Inspector (👁 visible).

```lua
node.visible = false                       -- hide it
if input.pressed("h") then node.visible = not node.visible end
```

> These work through the **node handle** too, so a manager script can re-skin any node it
> reaches: `find("Player"):getchild("Body").model = assets.getFile("models/hurt.glb")`.

### `node.enabled` — switch a node off entirely

Stronger than `visible`. A disabled node doesn't draw, doesn't collide, and its
scripts don't run — **and neither does anything below it**, so one call turns off a
whole room, weapon loadout or debug rig.

```lua
find("Tutorial Room").enabled = false      -- the room, its props and their scripts
find("Boss").enabled = true                -- and back
```

Also on the node's right-click menu in the Hierarchy (⏵ Disable), where a switched-off
node greys out, and it saves with the scene.

> **A node can't re-enable itself** — its scripts aren't running to do it. Something
> else has to, which is the same rule as any other object you've turned off.

### `node:getcomponent(name)` — tweak component fields live

Every tunable the Inspector shows on a **Rigidbody** or **Point Light** is also
scriptable. `node:getcomponent(name)` returns a **component handle** (or `nil` if the
node doesn't have that component): read a field to sample it, assign one to change it.
Writes apply the same frame — during Play the physics sim re-reads the body tunables
every step, so a change takes effect immediately with no reset or teleport.

| `getcomponent("RigidBody")` | Meaning (Inspector: ◆ Rigidbody) |
|---|---|
| `friction` | Surface friction 0..1 (0 = frictionless — ice). |
| `restitution` | Bounciness 0..1 (0 = no bounce). |
| `gravity` | Gravity pull on this body (assign `true`/`false`; reads back 1/0). |
| `shape` | Body shape: 0 = sphere, 1 = capsule, 2 = box. |
| `radius` | Sphere/capsule radius. |
| `height` | Capsule total height. |
| `half_x` `half_y` `half_z` | Box half-extents. |
| `lock_x` `lock_y` `lock_z` | Freeze world-axis translation (e.g. lock Z for 2.5D). A lock engaging mid-play freezes the body **where it is right then**. |
| `lock_rot_x` `lock_rot_y` `lock_rot_z` | Freeze rotation about an axis (keep a body upright). Holds the rotation the node has when the lock engages. |

| `getcomponent("PointLight")` | Meaning (Inspector: ● Point Light) |
|---|---|
| `intensity` / `range` | Brightness multiplier / reach in world units. |
| `r` `g` `b` | Light color, 0..1 per channel. |

| `getcomponent("Camera")` | Meaning (Inspector: ⌖ Camera) |
|---|---|
| `fovY` | Vertical field of view, radians. |
| `active` | The play-mode view camera — assign `true` to switch to it (a scripted camera cut). |

| `getcomponent("Material")` | Meaning (Inspector: ◑ Material) |
|---|---|
| `cell` | **Spritesheet frame**: which cell of the sliced base texture this surface draws (row-major from the top-left; clamped into the grid). The one material field cheap enough to write every tick. |
| `sheetCols` / `sheetRows` | The grid the texture is sliced into. Normally authored in the Inspector — it's inherited from the texture's own asset settings — so scripts only touch `cell`. |

Sprite-animating a mesh is that field and a clock — a character's face on a plane,
an animated billboard, a flipping coin:

```lua
local face, fps, frames, t = nil, 8, 16, 0

function start(node) face = node:getcomponent("Material") end

function update(node, dt)
  t = t + dt
  face.cell = math.floor(t * fps) % frames        -- or `base + i` per emotion
end
```

Everything else about a material (colors, textures, emissive) goes through
`node:setMaterial{...}` below — which also accepts `cell` / `sheetCols` /
`sheetRows` for setup-time slicing.

Booleans can be written as `true`/`false` (they read back as 1/0). All fields are
numbers — anything else raises a script error naming the field.

```lua
function update(node, dt)
  local rb = node:getcomponent("RigidBody")
  if rb then
    rb.friction = on_ice and 0.02 or 0.6   -- slide across the frozen lake
    if input.pressed("g") then rb.gravity = not (rb.gravity > 0) end
  end
end
```

> Handles work cross-node too: `find("Crate"):getcomponent("RigidBody").restitution = 0.9`.

### Game UI from scripts: `node.text` + the `Ui*` handles

UI elements are ordinary nodes, so the same handle mechanism drives HUDs. The string
side is a node property; everything numeric goes through `getcomponent`:

```lua
function start(node)
  -- cache in start (see §8) — find() every frame is wasteful
  hpLabel = find("HpLabel")
  hpBar   = find("HpBar")
end

function update(node, dt)
  hpLabel.text = hp                                   -- numbers coerce to text
  hpBar:getcomponent("UiSlider").value = hp           -- the Fill/Handle parts follow
  local el = hpBar:getcomponent("UiElement")
  el.opacity = hp < 20 and (0.5 + 0.5 * math.sin(time * 8)) or 1   -- low-hp flash
end
```

| Handle | Fields |
|---|---|
| `node.text` | The element's label text — read/write; writing a number is fine (`label.text = 42`). `nil` on nodes without a UI text. Writing to a UI element without a text spec creates one. |
| `node.texture` | The element's image texture, as a project asset path — read/write (`slot.texture = "textures/ui/portrait.png"`). `nil` on elements with no image; writing to one without an image slot creates it, so a bare element becomes a sprite. Raises if you assign something that isn't a string. |
| `getcomponent("UiElement")` | `visible` (1/0), `opacity`, `posX` `posY` (free position or pin offset, design units), `width` `height` (the number in the axis's sizing mode: px value, % fraction, or grow weight; `nil` on a *fit* axis — writing one makes it fixed px), `radius`, `border`, `fillR/G/B/A`, `textSize`, `textR/G/B/A`, `tintR/G/B/A`, `scrollY` (scroll views only: scroll position, 0 = top). |
| `getcomponent("UiSlider")` | `value`, `min`, `max` — on a slider (track) element. `value` is clamped to the range at draw time. |
| `getcomponent("UiLayer")` | `enabled` (1/0 — an off layer draws nothing), `z`, `designHeight`. |

Handles are `nil` when the node lacks the component — a node without an Element spec
has no `"UiElement"`, only slider tracks have `"UiSlider"`, only layers have
`"UiLayer"`.

> **`textSize` is not like the others.** `opacity`, `posX` and `tintR` are free to
> animate: they change numbers the GPU already has. **A text size is a cost.**
> Glyphs are rasterized and cached per `(font, character, pixel size)`, so every
> distinct size a project asks for buys a whole alphabet — and it is the *pixel*
> size, so a layer authored at `designHeight: 720` and played at 1440p rasterizes
> a second complete set at double the size.
>
> Writing `el.textSize = x` from `update` therefore rasterizes an alphabet **per
> intermediate value**: a half-second size pop is ~30 alphabets at the largest
> size in the project, for one flourish.
>
> The atlas grows rather than failing (and reports what it dropped), so this is a
> memory and hitching cost, not lost text — but it is a real one. For a size
> transition, animate **`scale`** instead: it is a vertex transform on glyphs that
> are already cached, and it costs nothing. Pick a small set of text sizes and
> reuse them across screens.

### Shader-drawn elements (`stage ui` .flsl) & `setShaderParam`

A UI element can carry a **custom shader face**: set its `shader` to a
`stage ui` `.flsl` file and the element's rect is drawn by that shader —
procedural instruments (the solar demo's navball, gauges, radar sweeps) with
no textures involved. Inside the shader you get `uv` (0..1 across the rect),
`instanceColor` (the element's tint × opacity) and `time`; `output color`'s
alpha shapes the element.

Scripts drive the shader's `uniform`s per tick — on UI elements AND on mesh
Materials with a shader — via:

```lua
navball:setShaderParam("nose", x, y, z)   -- vec3 (unset lanes are 0)
crystal:setShaderParam("glow", 2.5)       -- float
```

Each call is a GPU uniform write, never a recompile — per-tick driving is the
intended use.

### Editor actions & the construction API

Scripts can be **editor tooling**, not just gameplay — the Unity
editor-script analog. Declare a button:

```lua
--@editorButton Generate roll
function roll(node)
  -- runs in EDIT mode against the OPEN scene when clicked
end
```

and the Inspector shows **▶ Generate** on that script component. Clicking
runs exactly that function (never `start()`/`update`) with the node's
Inspector-tuned `params`; everything it does — transform and component
writes, `spawn`/`destroy`, and the construction API below — lands in the
edited scene as one undo step. The solar demo's `system_generator.lua`
(a "System Generator" node in the system scene) rebuilds its entire star
system this way; the engine only provides the generic pieces.

**Construction API** — build content from script, in actions or at runtime:

```lua
createNode("Oria", function(n)          -- a plain node (optional parent arg)
  n:setTerrain(2)                       -- make it a terrain volume (id 2)
  n:setCelestial{ mu = 5e5, parent = "Sun", a = 9000, atmoColor = {0.4,0.6,0.9} }
  n.x, n.y, n.z = 9000, 0, 0
  n.tags = { "genbody" }                -- tag your work so regenerating is safe
  createNode("Oria Core", n, function(core)   -- nested creates are fine
    core:setPrimitive("Sphere", {1, 0.5, 0.2})
    core:setMaterial{ unlit = true, emissive = {1, 0.45, 0.15}, emissiveStrength = 2.5 }
  end)
end)
terrain.generatePlanet(2, { radius = 180, caveDepth = 60, seed = 41 })
```

`setCelestial` also takes `occluderRadius` — occlusion culling for solid
bodies: the radius of a ball at the node's center that geometry never pierces
(a planet's core below its deepest cave). Terrain chunks fully hidden behind
it skip their draw calls, so the far side of a planet costs nothing. Keep it
conservative — below anything diggable — and `0` (the default) turns it off.

`setCelestial` / `setMaterial` create the component when absent and take
camelCase fields. A colour takes any of `{r,g,b}`, `{x,y,z}`, `{1,0.5,0.2}` or
`vec3(...)`, whichever reads best where you are.

**`setMaterial` is a setup-time call, not a per-frame one.** It inserts the
component and queues a deferred write, so driving a hit flash or a fade with it
every tick does far more work than you want. Write it on transitions (when the
flash starts and when it ends) and use `setShaderParam` — which writes a live
uniform — for anything that changes every frame.

`terrain.generatePlanet` is the heavy
generic primitive — a layered, cavernous, cratered sphere written into the
terrain field on a background thread (every knob optional; see the IDE hover
for the full list). `rng()` with no seed rolls a fresh stream from the clock
(`r.seed` reproduces it).

**Streaming worlds (galaxy scale)** — instead of pre-generating every body,
attach the *recipe* and let the engine generate it when someone actually goes
there (`docs/galaxy-streaming-proposal.md`):

```lua
n:setTerrain(2)
n:setTerrainGen{ radius = 180, caveDepth = 60, seed = 41 }  -- same opts table
```

A body with a genspec needs **no terrain file at all**: its field generates on
a background thread the first time anything approaches (deterministic per
seed), streams in chunk meshes as it lands, and streams back out — saving any
edits first — when you leave. A freshly rolled system is playable in seconds
however many worlds it has; unvisited worlds cost one scene node. Far bodies
always render as their correctly-colored impostor sphere, so nothing pops.

**Save slots** — `terrain.saveDir("saves/slot1/terrain")` points terrain
persistence at the player's save slot: streaming loads fields from there first
(before the project file or the genspec) and writes player-edited fields back
there on stream-out — so digs persist per slot without ever touching the
authored project. Pass `""` to clear; the slot resets when Play stops. Combine
with the `save.*` store (which holds the galaxy seed + progress) for the full
save-game loop: seed regenerates the untouched universe, the slot's terrain
dir carries exactly the worlds the player changed. `terrain.flush()`
checkpoints every edited resident field to the slot — **in the background**:
the field encodes a few chunks per frame and the file writes on a thread, and
a field the player dug within the last couple of seconds waits for a quiet
moment first, so an autosave loop never stutters the game. Exit paths (Stop,
`scene.load` out of the slot) finish outstanding writes synchronously — a
requested checkpoint is never lost. Call it freely on a timer.

**Deleting a save** — pair the two stores:

```lua
save.deleteSlot("slot2")                      -- the key→value store file
terrain.deleteSaveDir("saves/slot2/terrain")  -- that slot's persisted terrain
```

`save.deleteSlot` on the *active* slot also empties the in-memory store, so
the slot is instantly reusable as a fresh save. `terrain.deleteSaveDir` is
deliberately narrow — relative path, no `..`, never the active `saveDir`, and
it only removes terrain files (`.cfield`/`.tfield`/`.meta`) from that one
directory (tidying emptied directories after) — a save-management UI can call
it without any chance of eating unrelated files.

**The full player flow** (the solar demo implements this — `menu.ron` +
`game_manager.lua` are the reference):

```
main menu (menu.ron)          the game scene (system.ron)
  slot buttons ──save.slot──▶  game_manager.start():
                                terrain.saveDir("saves/<slot>/terrain")
                                seed = save.get("g_seed") or roll-and-store
                                show loading overlay
                                generator.regenerate(seed)   -- deterministic
                               game_manager.update():
                                hold the player above the spawn planet until
                                terrain.query(surface) answers → place them
                                (saved position if any), hide the overlay
                               ☰ MENU button → saveGame() → scene.load("menu")
```

The active `save.slot(...)` persists across `scene.load`, so the slot IS the
scene-to-scene handoff. Positions save RELATIVE to the dominant body (absolute
coordinates go stale when orbital phases restart) — restore places you at the
body's live position + offset. `terrain.generatePlanet` works at runtime too:
fills queue to the background generator and adopt with live collision, which
is what lets a loading screen rebuild a whole galaxy mid-session.

### 3D lines (`draw.line`)

Scripts can draw **world-space 3D lines** — the runtime line layer behind the
solar demo's KSP-style map (orbit conics, SOI rings, markers) and any debug
overlay you like:

```lua
draw.line(a.x, a.y, a.z, b.x, b.y, b.z, 0.3, 0.85, 1.0)        -- rgb
draw.line(x1, y1, z1, x2, y2, z2, 0.5, 0.5, 0.6, 0.4)          -- + alpha
```

### Screen-space shapes & text (`draw.rect`, `draw.circle`, `draw.text`)

Immediate-mode drawing in **pixels** — the same pixels `input.mouse()` reports:

```lua
draw.rect(x, y, w, h, r, g, b [, a] [, radius])        -- filled
draw.rectOutline(x, y, w, h, r, g, b [, a] [, px])     -- hollow, `px` thick
draw.circle(x, y, radius, r, g, b [, a])               -- x,y is the CENTRE
draw.circleOutline(x, y, radius, r, g, b [, a] [, px])
draw.text(x, y, s, size, r, g, b [, a] [, align])      -- align: "left"|"center"|"right"
```

`draw.text` is measured and laid out by the engine with the same font stack
`ui.make` uses, so a damage number, a frame-time readout or a count under a
selection box needs no UI tree and no idea how wide an `m` is. `align` says
which edge `x` is:

```lua
-- a HUD in three lines
draw.text(24, 24, "HP " .. hp, 22, 1, 0.4, 0.4)
draw.circle(40, 80, 12, 0.3, 1, 0.5, 0.8)
draw.text(w - 24, 24, string.format("%.1f fps", 1 / dt), 18, 1, 1, 1, 0.7, "right")
```

They draw over the scene *and* over the HUD, in the Game view and in a build.
This is the whole of an RTS marquee — the two corners you dragged between:

```lua
function update(node, dt)
  local mx, my = input.mouse()
  if input.clicked(0) then press = { x = mx, y = my } end
  if press and input.button(0) then
    local x, y = math.min(press.x, mx), math.min(press.y, my)
    local w, h = math.abs(mx - press.x), math.abs(my - press.y)
    draw.rect(x, y, w, h, 0.35, 1.0, 0.55, 0.12)        -- translucent fill
    draw.rectOutline(x, y, w, h, 0.45, 1.0, 0.6, 0.9, 1.5)
  end
  if press and not input.button(0) then
    -- …and a thing is "in the box" when `camera.worldToScreen` puts it there.
    press = nil
  end
end
```

Doing the same job with 3-D lines means projecting a rectangle onto a ground
plane, which fights the camera angle and misses anything the plane doesn't pass
through. `rts_commander.lua` is the worked example.

Immediate mode: a segment lives **one frame** — keep calling it while you want
it visible (an idle script's lines vanish by themselves). Draw from
`lateUpdate` when the lines belong to a camera you position there (the solar
map does): it runs in the camera pass, so the lines land the same frame as the
camera. Lines draw **over** the scene — never occluded, the way KSP orbit
lines read through planets — and render in every game view.

### Buttons & pointer hooks

Turn on **button (clickable)** on any element (or Add ⏵ UI ⏵ Button) and its
scripts get pointer hooks — plain functions, called with a node handle:

| Hook | Fires |
|---|---|
| `hoverStart(node)` / `hoverEnd(node)` | the pointer entered / left the element |
| `pressed(node)` / `released(node)` | LMB went down on it / came back up |
| `clicked(node)` | pressed AND released on the same element |
| `focusEnter(node)` / `focusExit(node)` | keyboard/gamepad focus arrived / left |
| `cancelled(node)` | `UiCancel` (Escape / B) while focused |
| `changed(node)` / `submitted(node)` | a text field's value changed / Enter |
| `dragStart` / `dragMove` / `dropped` / `dragCancel` | on a `draggable` source |
| `dragEnter` / `dragOver` / `dragLeave` / `dropped` | on a `drop target` |

A gamepad **submit fires the same `clicked`** a mouse does, so a button written
for a pointer works with a pad and no second code path. See
[ui-navigation.md](ui-navigation.md) for focus, text fields, drag & drop and
tooltips, and [ui-styles.md](ui-styles.md) for what the states look like.

```lua
ui.focus(find("Play"))    ui.focused()      -- move / read the focus
ui.dragging()             ui.dropTarget()   -- the drag in flight
```

### One script for a whole screen — `ui.on` & `ui.events`

A `clicked` function answers for the node its script is on. A menu of eight
buttons therefore wants eight script files, each three lines long, each really
saying *tell the menu* — and the state they all change lives somewhere else
again. Two ways to keep a screen in one script instead.

**Listen from anywhere.** `ui.on(element, hook, fn)` registers a handler from a
script that does not live on the element:

```lua
function start(node)
  ui.on(find("Play"),    "clicked", function() scene.load("level1") end)
  ui.on(find("Options"), "clicked", function() find("OptionsPanel").visible = true end)
  ui.on(find("Quit"),    "clicked", function() scene.load("title") end)
end
```

The handler is called `fn(element, hook)` — the element that fired and the hook
name — so one function can serve a whole row:

```lua
for _, b in ipairs(find("Toolbar"):children()) do
  ui.on(b, "clicked", function(el) selectTool(el.name) end)
end
```

Every hook in the table above works. Four rules make it safe to write:

- **Registering again replaces.** Same script, same element, same hook — the new
  closure takes the old one's place, so calling `ui.on` from `update` costs one
  closure rather than one per frame.
- **`ui.off(element)` stops every hook your script has on it**; `ui.off(element,
  "clicked")` stops one. Only *yours*: two managers listening to one button can
  never unregister each other.
- **A listener dies with either end** — the element it watches or the script that
  registered it. A destroyed menu manager stops answering, and a hot reload
  re-registers from the fresh code.
- **Order:** the element's own `clicked` function runs first, then a `ui.make`
  element's inline `onClicked`, then listeners in registration order.

Listening for an interaction an element does not take (a `clicked` on a plain
box) warns in the Console. Nothing else would happen at all, and silence is a
bad error message.

**Or ask, instead of being called.** The same events, polled in `update`:

```lua
function update(node, dt)
  if ui.clicked(playButton) then start() end
  for _, ev in ipairs(ui.events("clicked")) do
    log("clicked " .. ev.node.name)
  end
end
```

| Call | Answers |
|---|---|
| `ui.clicked(el)` / `pressed` / `released` / `changed` / `submitted` | did it fire this frame? |
| `ui.event(el, hook)` | any hook, by name |
| `ui.events([hook])` | everything that fired this frame: `{ node = , event = }` |
| `ui.hovered([el])` / `ui.held([el])` / `ui.focused([el])` | which element — or, given one, yes/no |

The last row is **states**, not events: true for as long as they are true, where
`hoverStart` / `hoverEnd` are the edges. Everything else is per-frame and gone
the next.

Polls and hooks read the same list, published before scripts run, so the two can
never disagree about what happened this frame.

### Colours

`color(r, g, b [, a])` — channels 0..1, alpha 1 by default, so `color(1, 0, 0)`
is opaque red rather than invisible red. Also `color(gray)`,
`color(other, 0.5)` to copy with a new alpha, `color.hex("#ff8800")` and
`color.lerp(a, b, t)`. It's a plain `{r, g, b, a}` table (also `[1]`..`[4]`),
so it prints, saves into a file and compares — and a `{1, 0, 0}` you already
had lying around is already a colour.

```lua
local el = node:getcomponent("UiElement")
el.fill = color.hex("#1b1e26")
el.textColor = color.lerp(dim, bright, t)
```

Whole-colour fields: `fill`, `textColor`, `borderColor`, `tint`, `groupTint`,
`caretColor`, `selectionColor`, `placeholderColor`. The per-channel names
(`fillR`…) still work — a script that fades one channel is untouched.

Boolean fields (`visible`, `disabled`, `selected`, `toggle`, `focusable`,
`gravity`, `kinematic`, `active`, `enabled`, the `lock_*` set) now read back as
**real booleans**. They used to read back as 1/0, and `0` is truthy in Lua —
`if el.visible then` was always taken. If you were comparing one with `> 0`,
drop the comparison.

### Bindings — `ui.bind`

```lua
ui.bind(params.coins, "text",  function() return ("%d ¢"):format(coins) end)
ui.bind(params.hpBar, "value", function() return hp / maxHp end)
ui.bind(params.warn,  "textColor", function() return hp < 20 and red or white end)
```

Say the relationship once instead of writing an `update` that keeps it true.
The engine calls the function once a frame — **after** every `update`, so a
label shows this frame's value, not last frame's — and writes what comes back.

Which component it writes to is decided by which one actually *has* that field,
so `"value"` finds `UiSlider` and `"opacity"` finds `UiElement` without you
saying. Returning `nil` means "nothing to say this frame", not "write zero".
Re-binding the same property replaces it; two functions fighting over one label
every frame is never what was meant.

A binding whose node is gone is dropped silently (a screen closing is not an
error). One that **throws** is dropped after reporting once — left in place it
would report the same failure sixty times a second and bury everything else.
`ui.unbind(node)` drops them all, `ui.unbind(node, "text")` just one.

### Lists — the repeater

Tick **repeat a row** on a container, name a row prefab, and drive `count`:

```lua
ui.bind(params.list, "count", function() return #inventory end)
```

The engine keeps the container's children matching `count`, spawning and
destroying only the **difference** — a list that gains a row keeps the other
nine, with their script state, their hover, their in-flight style transitions
and the view's scroll position. Rebuilding the lot every frame is what makes a
hand-rolled list flicker and forget.

Each row reads `node.index` (0-based, in flow order) and fills itself in:

```lua
-- on the row prefab
function update(node, dt)
    local item = inventory[node.index + 1]
    if item then node.text = item.name end
end
```

`node.index` is `nil` on anything a repeater didn't spawn, so `if node.index`
is a fine "am I a row". Repeaters run **during Play only** — the rows are
runtime entities, and conjuring them in edit mode would put engine-spawned
nodes into a scene you're about to save. Put one row in the scene by hand to
design against and let the repeater fill the rest.

### Screens from data — `ui.make`

A repeater answers "there should be N of these". When the SHAPE of the screen
comes from data — not just how many rows, but what they contain — describe it:

```lua
ui.make(find("Crew Panel"), {
    "col", inset = 0, style = "panel", gap = 10, pad = 16,
    { "text", text = "CREW · " .. #crew .. " on duty", style = "caption" },
    { "col", w = "100%", gap = 6, items = crew,
        function(m)
            return {
                "button", key = m.id, style = "row", dir = "row", gap = 10,
                onClicked = function() standDown(m.id) end,
                { "box", w = 26, h = 26, radius = 13, text = m.name:sub(1, 1) },
                { "text", text = m.name },
            }
        end,
    },
})
```

Full manual: **[ui-make.md](ui-make.md)**. The short version:

- An element is `{ "kind", prop = value, …, children }`. Kinds:
  `box`, `row`, `col`, `text`, `image`, `button`, `field`, `slider`, `scroll`.
- `items = {…}` plus a function child makes **one child per item** — the
  function gets `(item, i)` and may return `nil` to skip. A function child
  *without* `items` is a conditional part of the screen.
- `onClicked = function(node) … end` — any UI hook, `on` + its name — carries
  behaviour inline. No prefab, no second file.
- **Call it again when the data changes.** It reconciles: only the difference
  is spawned and destroyed, so the rows that stay keep their entity, their
  hover, their scroll position and their in-flight transitions. `key = "id"` is
  how a row keeps all that through a re-sort.
- The description is authoritative — a property you stop mentioning goes back
  to default. What the *player* did (scroll, typing, a toggle, a dragged
  slider) is kept, because that isn't something the description said.
- Elements you placed by hand under the same container are never touched, so a
  data-driven list can live inside a designed panel.
- Play only, same as the repeater. A mistyped property **raises** — a
  declarative screen that silently ignores a line is worse than one that stops.

The engine imposes no button look — style the states yourself, it's 5 lines:

```lua
function hoverStart(node)  node:getcomponent("UiElement").opacity = 0.8 end
function hoverEnd(node)    node:getcomponent("UiElement").opacity = 1.0 end
function clicked(node)     log("play pressed!") end
```

A slider with **draggable** on lets the player click/drag the track to set its
value — read it with `getcomponent("UiSlider").value` (a settings volume slider
is a draggable slider + one `update` that reads the value). Display-only meters
(health bars) leave it off.

### Scroll views

An element with the **scroll view** option (Add ⏵ UI ⏵ Scroll View, or the
Inspector checkbox) turns into a wheel-scrollable viewport: put more content
inside than fits and it clips to the element's rounded rect and scrolls —
children keep their authored layout, rows scrolled out of view neither draw
nor click, and the wheel only reaches gameplay when the pointer isn't over a
scroll view. The offset is clamped to the content, so a view whose content
fits doesn't scroll at all. Scripts read/write it as
`getcomponent("UiElement").scrollY` (design units, `0` = top — reset it when
you re-open a panel). The solar demo's New Galaxy panel is the reference: a
`Scroll View` holding one slider row per generator parameter.

## 8. Referencing other nodes & scripts

A script isn't limited to its own node. You can **walk the hierarchy**, **find any
node or script in the scene**, and **call into another script** — read its state, set
its values, invoke its methods. This is how you build systems that span many scripts:
a single **manager** holding shared state, with other scripts handing data to it.

### Reaching other nodes

The `node` you're given (and any node you reach) is a **handle**. Handles share the
same fields as your own `node` (`x/y/z`, `yaw/pitch/roll`, `scale`, and `vx/vy/vz`,
`grounded`, … on rigidbody nodes), so you can read and write another node's transform
the same way.

| On a node handle | Returns |
|---|---|
| `node.name` | the node's name (string) |
| `node.id` | a stable numeric id for this node |
| `node.parent` | the parent node handle, or `nil` |
| `node:getparent()` | same as `node.parent` |
| `node:children()` | an array (`{1,2,…}`) of child handles |
| `node:getchild("Gun")` | the first child named `Gun`, or `nil` |
| `node:find("Muzzle")` | the first **descendant** (any depth) with that name, or `nil` |
| `node:getscript("health")` | a **script handle** for that script on this node, or `nil` |

Scene-wide lookups are globals:

| Global | Returns |
|---|---|
| `find("Player")` | the first node in the scene with that name, or `nil` |
| `findAll("Coin")` | an array of every node with that name |
| `findScript("GameManager")` | a **script handle** for the first node anywhere running that script (the manager pattern), or `nil` |
| `findScripts("third_person")` | an array of script handles — EVERY node carrying that script, in scene order (pair with `net.isMine` to pick the local player among many avatars) |

`find()` is an O(1) hash lookup (the engine keeps a name index), so it's cheap —
but caching a handle in `start` is still the cleanest habit for per-frame use.

```lua
-- A door that opens when the player is near it.
function update(node, dt)
  local player = find("Player")
  if not player then return end
  local dx, dz = player.x - node.x, player.z - node.z
  if dx*dx + dz*dz < 9 then node.y = 3 else node.y = 0 end   -- raise / lower
end
```

### Node references — wire them in the Inspector, skip `find()` entirely

Declare a `defaults` entry as `noderef()` and the Inspector shows a **node
picker** for it. The script reads the param as a ready node handle:

```lua
defaults = { target = noderef(), speed = 2 }

function update(node, dt)
  if params.target then                 -- nil while unwired (or the node is gone)
    node.yaw = math.atan2(params.target.x - node.x, params.target.z - node.z)
  end
end
```

This is the preferred way to point a script at a specific node: no name typos in
code, no lookups, and re-wiring is a dropdown pick instead of an edit — or just
**drag a node from the Hierarchy onto the slot**. The reference resolves by name
each tick, so a target spawned or renamed mid-play binds automatically.

Want the thing ON the node rather than the node? Declare the kind and skip the
`getcomponent`/`getscript` chain entirely:

```lua
defaults = {
  victim = scriptref("health"),        -- that SCRIPT on the wired node
  body   = componentref("RigidBody"),  -- that COMPONENT on the wired node
}

function update(node, dt)
  if params.victim then params.victim.damage(10) end   -- a script handle
  if params.body then params.body.friction = 0.05 end  -- a component handle
end
```

The Inspector filters the picker to valid targets — `scriptref("health")` only
lists nodes carrying a `health` script, `componentref("RigidBody")` only nodes
with a Rigidbody (and a dragged node is rejected with a red outline if it
doesn't qualify). Referenceable components: `RigidBody`, `PointLight`,
`Camera`, `ParticleSystem`, `UiElement`, `UiSlider`, `UiLayer`. Unwired or
invalid references read `nil`.

### Reaching other scripts

A **script handle** (from `node:getscript(name)` or `findScript(kind)`) lets you talk
to another script:

| On a script handle | Meaning |
|---|---|
| `mgr.score` | read a variable the script declared (its state) |
| `mgr.score = 10` | write that variable |
| `mgr.addScore(5)` | **call a function** the script defines |
| `mgr.params` | the script's `params` table (its tunables) |
| `mgr.node` | the node the script is attached to (a node handle) |

```lua
-- scripts/manager.lua — shared state + an API for other scripts to call.
score = 0
function addScore(n)
  score = score + n
  log("score: " .. score)
end

-- scripts/coin.lua — on pickup, hand the points to the manager.
function update(node, dt)
  if picked_up then
    local mgr = findScript("manager")
    if mgr then mgr.addScore(10) end
  end
end
```

Inside a script's own functions, `node` always refers to **its** node (so a method
called from elsewhere still acts on the right object), and `params` is its tunables.

### Where is it *really*? — `node.worldX/worldY/worldZ`

`node.x/y/z` are **local**: for a child, they are measured from its parent. That
is what you want when you move something, and exactly what you don't want when
you compare it against a world-space target:

```lua
-- Read-only, and composed up the whole parent chain (position, rotation, scale).
local wx, wy, wz = node.worldX, node.worldY, node.worldZ
local here = node.worldPos                     -- …or all three as a vec3

-- Am I there yet? Measure in WORLD space, always:
if distance(here, target) < 1.0 then arrived() end
```

A unit under a container node that compares `node.x` against a world order never
arrives — it walks past it and keeps going. Use `worldX/Y/Z` for distances,
targets, and anything you hand to another script; use `x/y/z` to move.

> **Notes.** Node handles expose a node's **local** transform (the same values as the
> `node` argument). `findScript` returns the *first* matching script — perfect for a
> single manager. Looking something up by name? Cache it in `start` and reuse it; a
> handle stays valid across frames.

### Recipe: a first-person HUD that follows the camera mode

The stock `third_person_camera.lua` exposes its state as script globals —
`cam.firstPerson`, `cam.shiftlock` — exactly so other scripts can react to the
view mode. Put your HUD elements under a **UI Layer** node, attach this, and
the layer shows only in first person:

```lua
-- scripts/fp_hud.lua — attach to the UI Layer node holding the HUD.
local cam

function update(node, dt)
  if not cam then cam = findScript("third_person_camera") end
  local layer = node:getcomponent("UiLayer")
  if layer and cam then
    layer.enabled = cam.firstPerson and true or false
  end
end
```

The same pattern reads anything the camera knows: `cam.params.distance` for a
zoom readout, `cam.shiftlock` for a crosshair, and so on.

## 9. Animation: `node:animator()`

Any node with an **Animation Controller** component (or a rigged model with
embedded clips) exposes an animation handle. See `docs/animation.md` for the
full system (controllers, layers, events, the stepped retro look).

```lua
local anim
function start(node)
  anim = node:animator()
end

function update(node, dt)
  local speed = math.sqrt(node.vx^2 + node.vz^2)
  if not node.grounded then anim:play("Jump")
  elseif speed > 6     then anim:play("Run")
  elseif speed > 0.5   then anim:play("Walk")
  else                      anim:play("Idle") end

  if input.pressed("j") then anim:restart("Slash") end -- one-shot attack layer
end

-- called by a ⚑ event key placed on a clip's timeline:
function onSlashHit(node) log("hit frame!") end
```

| Call | What it does |
|---|---|
| `anim:play(state [, fade [, layer]])` | transition (controller decides the fade; safe every frame) |
| `anim:restart(state [, fade [, layer]])` | force re-entry (re-trigger a one-shot) |
| `anim:crossfade(state, fade [, layer])` | transition with an explicit fade |
| `anim:stop([layer [, fade]])` | stop a layer (all if omitted) |
| `anim:setSpeed(x)` | global speed multiplier |
| `anim:setLayerWeight(layer, w)` | blend a layer over the ones below (0..1) |
| `anim:seek(t [, layer])` | jump the playhead |
| `anim:state([layer])` / `anim:time([layer])` | what's showing / seconds in (`anim:current` is an alias of `anim:state`) |
| `anim:finished([layer])` | a one-shot reached its end |
| `anim:isPlaying([state])` | is a state (or anything) playing |
| `anim:clips()` / `anim:layers()` | available state / layer names |
| `anim:duration(clip)` / `anim:events(clip)` | the clip **as authored** — length in seconds, and its event list |

**`anim:duration` / `anim:events` read the asset, not playback**, so they answer
in `start()`, before anything has played a frame. `events` returns
`{ {t = seconds, func = "onHitboxStart"}, ... }` ascending by `t` — an unknown
clip is `nil`, a clip with no events is an empty list.

They exist so an animator can author timing **by eye** while the game still runs
on integer frames. Drop an event on the frame where the strike connects, then
bake it once at load:

```lua
local dur = anim:duration(clipName)
for _, e in ipairs(anim:events(clipName) or {}) do
  if e.func == "onHitboxStart" then
    move.startup = math.floor(e.t / dur * move.frames + 0.5)
  end
end
```

Don't let events *drive* frame-exact gameplay directly: they fire off float
playback time, stepped playback (`sample_fps`) quantises them to its grid, clip
time and gameplay frame disagree mid-crossfade, and a prediction replay
deliberately doesn't re-fire them. Every machine loads the same `.anim.ron`, so
a baked number is identical everywhere and constant thereafter.


**Conditional expressions.** Lua's `and`/`or` chain is an inline if/else —
handy for mapping states to values without an if-ladder:

```lua
local speed = anim:isPlaying("Running") and 2
           or anim:isPlaying("Walking") and 1
           or 0
```

The one gotcha: put the **condition first**. `a and b` yields `b` only when
`a` is truthy, so `2 and anim:isPlaying("Running")` gives you the *boolean*,
not the 2. (And this only picks non-false values — `cond and false or x`
always lands on `x`.) Method names are camelCase: `anim:IsPlaying` is an
error, and the animator will suggest the spelling it thinks you meant.

**Events → functions.** Put a ⚑ event on a clip in the **✎ Animating** tab and
name a function; when the playhead crosses it during Play, that function is
called (with the node) on every script attached to the controller's node that
defines it.

## 10. Particles: `node:particles()`

Any node with a **Particle System** component exposes a particle handle, so
scripts can fire and stop effects on cue — muzzle flashes, footstep dust,
thruster plumes, pickups. See `docs/particle-system-proposal.md` for authoring
effects on the ❋ Particles timeline.

```lua
function update(node, dt)
  local p = node:particles()

  -- one-shot burst on each shot (re-fires even mid-play):
  if input.clicked(0) then p:restart() end

  -- a continuous effect that follows a condition:
  local jet = find("Thruster"):particles()
  if input.key("w") then jet:play() else jet:stop() end

  if p:isPlaying() then log("smoke: " .. p:alive() .. " alive") end
end
```

| Call | What it does |
|---|---|
| `p:play()` | start emitting if idle (spawns a fresh instance); no-op if already playing |
| `p:stop()` | stop + despawn — the live particles vanish |
| `p:restart()` | re-spawn from `t=0` (re-fire a one-shot burst) |
| `p:setIntensity(i)` | live emission scale 0..~2 — throttle a plume without touching the asset |
| `p:setBeamEnd(x, y, z)` | aim every **Beam** track's endpoint at a WORLD point (converted to effect-local, so the beam tracks the target as the node moves) |
| `p:isPlaying()` | is an instance emitting/ageing right now |
| `p:alive()` | live particle count across the effect's tracks |
| `p:asset()` | the effect asset key this node references, or `nil` |

> Handles work cross-node: `find("Campfire"):particles():stop()`. A node's
> **Play on start** flag is also scriptable —
> `node:getcomponent("ParticleSystem").play_on_start = 1`.

### `spawnEffect` — fire a one-shot at a world point

For hits, pickups, footstep poofs — effects that aren't tied to a node — spawn
one anywhere in the world and forget it. It plays once and despawns itself:

```lua
function update(node, dt)
  if input.clicked(0) then
    local h = raycast(node.x, node.y, node.z, fx, fy, fz, 100)
    if h then spawnEffect("vfx/Impact", h.x, h.y, h.z) end
  end
end
```

`spawnEffect(key, x, y, z)` — `key` is the effect asset (project-relative, no
`.vfx.ron`); the position is world space. Author it as a **one-shot** effect on
the ❋ Particles timeline so it ends cleanly. That's the whole loop: design it on
the timeline → `spawnEffect` it from gameplay.

## 11. Audio: `audio.play`, `node:sound()` & the mixer

Playing a sound needs nothing but a clip path — no prefab, no source node, no
spawn-then-get-component dance:

```lua
audio.play("audio/ding.ogg")                          -- flat 2D (UI, stingers)
audio.play("audio/hit.ogg", h.x, h.y, h.z)            -- 3D at a world point
audio.play("audio/engine.ogg", carNode, {loop = true}) -- follows the node
```

Sounds default to **spatial**: they attenuate with distance and pan toward
where they are relative to the active camera. Every knob rides in the options
table (all optional):

```lua
local s = audio.play("audio/roar.ogg", bossNode, {
  volume = 0.8,             -- linear, 1 = as authored
  pitch = 1.1,              -- playback rate (also shifts pitch)
  mode = "Spatial",         -- "Distance" = attenuate only · "Flat" = plain 2D
  falloff = "Inverse",      -- "Linear" · "Exponential"
  minDistance = 2,          -- full volume inside this range
  maxDistance = 50,         -- silent past this range
  track = "SFX",            -- mixer track to route through (default Master)
  endBehavior = "Destroy",  -- "Stop" (default) · "Destroy" · "Loop"
})
```

`audio.play` returns a **sound handle**, live until the sound ends:

| Call | What it does |
|---|---|
| `s:stop()` | fade out (a few ms — never clicks) and end |
| `s:pause()` / `s:resume()` | freeze / continue playback |
| `s:setVolume(v)` / `s:setPitch(v)` / `s:setPan(v)` | live tweaks |
| `s:setTrack("Music")` | re-route through another mixer track |
| `s:setPosition(x, y, z)` | move the emitter (stops following a node) |
| `s:seek(secs)` | jump the playhead |
| `s:isPlaying()` / `s:position()` | playback state |

`endBehavior = "Destroy"` on a node-following sound despawns that node when
the sound finishes — spawn a node, hang a sound on it, and it cleans itself up.

### The Audio Source component

For authored emitters (ambient loops, music zones, alarm props), add an
**Audio Source** in the Inspector (➕ Add Component): pick the clip, spatial
mode, falloff, distances, mixer track, end behavior, and **Play on start**.
Scripts drive it through `node:sound()`:

```lua
local alarm = find("Alarm"):sound()
alarm:play()                     -- restart its clip
alarm:setClip("audio/alarm2.ogg")
alarm:pause()  alarm:resume()  alarm:stop()
if alarm:isPlaying() then log(alarm:position()) end
```

Its tunables mirror live through `getcomponent` (numbers only, like every
component):

| field | Meaning (Inspector: ♪ Audio Source) |
|---|---|
| `volume` | linear volume 0..2 |
| `pitch` | playback rate (0.5 = octave down) |
| `pan` | stereo pan −1..1 (Flat mode) |
| `minDistance` / `maxDistance` | the falloff range |
| `playOnStart` | 1/0 — play when Play starts |
| `mode` | 0 = Spatial · 1 = Distance · 2 = Flat |
| `falloff` | 0 = Inverse · 1 = Linear · 2 = Exponential |
| `endBehavior` | 0 = Stop · 1 = Destroy · 2 = Loop |

```lua
node:getcomponent("AudioSource").volume = 0.3   -- live while playing
```

### The mixer

Everything audible routes through the **🎧 Mixer** tab: named tracks with a
fader, pan, mute/solo, an effect chain (parametric EQ with a draggable curve,
delay, reverb, chorus, flanger, phaser, pitch shift, compressor, limiter,
distortion, utility), and routing — tracks can output into other tracks
(e.g. `Footsteps → SFX → Master`). The graph saves with the project
(`project.ron`); anything that doesn't name a track plays on **Master**.

Scripts get live control that reverts when Play stops:

```lua
audio.track("Music"):setVolume(-12)   -- duck music (fader dB)
audio.track("SFX"):setPan(0.2)
audio.track("Master"):setMuted(true)
audio.stopAll()                       -- silence everything
```

Clips are plain files under `assets/audio/` (`.wav`, `.ogg`, `.mp3`,
`.flac`) — double-click one in the Assets browser to preview it. Clip
references are project-relative paths (`"audio/hit.ogg"`; the extension may
be omitted).

## 12. Recipe: a walkable first-person character

No glue code required:

1. Add a **Camera** node and mark it **Active**.
2. Give it a **Rigidbody**, shape = **Capsule**.
3. Attach **`first_person.lua`**.

Press **Play** — you *are* the capsule. It moves under physics and the camera rides
along, so you walk the world in first person:

- hold **Right Mouse** — free-look (yaw + pitch)
- **W A S D** — move along the ground, relative to where you face
- **Space** — jump (when grounded)
- **Shift** — run · hold **C** — crouch

It works on normal **Down** gravity *and* **Radial** (planet) gravity — drop a
**Gravity Volume → Radial** node at a planet's center and you can run all the way
around it.

A minimal controller that shows the velocity loop:

```lua
defaults = { speed = 6, jump = 7 }

function update(node, dt)
  local f = (input.key("w") and 1 or 0) - (input.key("s") and 1 or 0)
  local vy = node.vy                                  -- keep gravity/jump
  if node.grounded and input.pressed("space") then vy = params.jump end
  node.vx = -math.sin(node.yaw) * f * params.speed
  node.vz = -math.cos(node.yaw) * f * params.speed
  node.vy = vy
end
```

## 13. Bundled example scripts

Every project ships these under `scripts/` — open one for a working start:

| Script | What it does |
|---|---|
| `first_person.lua` | First-person character (attach to an active Camera with a capsule Rigidbody: free-look, run, crouch, jump; planet-aware; slope-forgiving jump via a downward ground probe) |
| `third_person.lua` | Third-person character body (capsule Rigidbody + a child named `Model` for the visuals; camera-relative movement, auto-turns, drives Idle/Walk/Run/Jump — matches the controller's real state names, e.g. `Idle.001`; slope-forgiving jump) |
| `third_person_camera.lua` | Orbit camera for the third-person body (mouse orbits, scroll zooms, zoom all the way in for first-person freelook; raycasts so walls never clip the view) |
| `freelook.lua` | Free-fly camera (right-mouse look, WASD, Shift to boost) |
| `rts_camera.lua` | Isometric strategy camera (WASD/edge-of-screen panning, wheel zoom, Q/E rotate about the focus point, optional follow + map bounds) |
| `rts_unit.lua` | A commandable unit: `moveTo(x, y, z)` / `stop()` / `isMoving()`, physics-driven if the node has a Rigidbody, transform-driven if it doesn't, with a selection ring |
| `rts_commander.lua` | The mouse half of an RTS: click / drag-box to select (Shift adds), right-click the ground to send the selection there in a loose formation |
| `rotate.lua` | Spin a node about Y |
| `pulsate.lua` | Animate scale over time |
| `float.lua` | Bob up and down |

## 14. The in-engine IDE

Double-click a `.lua` in Assets (or use the Inspector's Scripting section) to
open it in the **Scripting** tab — a small but real code editor:

- **Find & replace** — `Ctrl+F` finds (seeded from your selection), `Ctrl+H`
  adds a replace row, `Enter` / `Shift+Enter` or `F3` / `Shift+F3` step
  through matches (the current one is outlined), `Aa` toggles match case, and
  **⌕ all scripts** lists every matching line across the whole project.
  Typing in the find field never yanks focus back into the code.
- **Line editing** — with nothing selected, `Ctrl+C` / `Ctrl+X` copy / cut the
  whole current line. `Ctrl+D` duplicates, `Ctrl+Shift+K` deletes,
  `Alt+Up/Down` moves the selected lines, `Ctrl+/` toggles `--` comments over
  the selection, and `Tab` / `Shift+Tab` indent / outdent a multi-line
  selection. `Enter` auto-indents (one level deeper after `then`/`do`/`function`).
- **Navigation** — `Ctrl+G` goes to a line, `Ctrl+B` (or right-click) jumps to
  a definition, right-click also finds all references. The Console's
  double-click-to-source lands here too.
- **Saving** — `Ctrl+S` saves, `Ctrl+Shift+S` saves all; closing a tab with
  unsaved changes asks first, and pressing **Play auto-saves** open edits so
  the run always matches what you see.
- **Completion & docs** — the popup opens **by itself only after `.` or `:`**,
  where you're asking what fields something has; `Ctrl+Space` summons it anywhere
  else. `↑`/`↓` choose, **`Enter` accepts**, `Esc` hides it until the token
  changes — and **`Tab` always indents**, so completion can never eat a
  keystroke you aimed at your code. It understands member access on **any
  variable** — `rb.fri` offers `friction`, `anim:pl` offers `play`, and
  `params.` offers this script's own `defaults` keys. The highlighted entry
  shows its doc *and a usage example* in the popup; hovering an API name in
  code shows the same. The **§ Docs** page has a search box over the whole
  guide + API reference, with worked examples under the common entries.
- **Formatting** — `Alt+Shift+F` (or the **⚏ Format** button, or tick **on
  save**) re-indents the file by block depth and tidies whitespace. It changes
  **nothing else** — no re-flowed expressions, no realigned comments, no moved
  code — and it's idempotent, so format-on-save can't produce a second diff.
  `--@noformat` exempts a file; a line ending in `--@keep` keeps its own
  indentation.
- **Warnings** — a `⚠ n warnings` strip under the editor expands into a
  clickable list of the mistakes Lua can't report:
  - **an undeclared assignment** — `sped = speed * dt` compiles, writes a
    global, reads `nil` forever, and says nothing. The warning names it and
    suggests the local you meant. Globals assigned at **file scope** are
    deliberate publications (§8) and are never flagged.
  - **an unused local** — usually a half-finished rename. Prefix with `_` to
    keep it quiet.
  - **upvalue pressure** — LuaJIT allows **60** upvalues per function and every
    file-scope `local` is one; at 50 you get a warning whose message names the
    fix (group related state into one table).
  - **a hook that forgot the node** — `function update(dt)` looks right and is
    wrong: every lifecycle hook is called with the **node first** (§3), so `dt`
    is bound to the node and the first arithmetic on it raises, every frame,
    before anything visible has happened. From the outside that isn't an error,
    it's a script that does nothing at all.

  `--@nolint` silences a line; on its own line it silences the file.

### Shortcuts

| | | | |
|---|---|---|---|
| `Ctrl+S` | save | `Ctrl+Shift+S` | save all |
| `Ctrl+F` | find | `Ctrl+H` | find & replace |
| `F3` / `Shift+F3` | next / prev match | `Ctrl+G` | go to line |
| `Ctrl+C` / `Ctrl+X` | copy / cut line | `Ctrl+D` | duplicate line |
| `Ctrl+Shift+K` | delete line | `Alt+↑` / `Alt+↓` | move line(s) |
| `Ctrl+/` | toggle comment | `Tab` / `Shift+Tab` | indent / outdent |
| `Ctrl+B` or `F12` | go to definition | `Shift+F12` | find references |
| `Alt+Shift+F` | format document | `Ctrl+Space` | suggest |
| `Ctrl+W` | close tab | | |

The same list lives on the tab's **§ Docs** page.

## 15. Tips & gotchas

- **Run, then Play:** scripts only execute while the game is playing (F1). Stop
  restores the scene to its pre-Play state.
- **Drive bodies by velocity, not position.** Setting `node.x/y/z` on a Rigidbody
  node fights the physics step; set `node.vx/vy/vz` instead.
- **Edges vs. held:** use `input.pressed` / `input.clicked` for one-shot actions
  (jump, fire) and `input.key` / `input.button` for held movement.
- **Errors** appear at the top of the Scripting tab and in the Console, with the
  script name + line — double-click to jump to the source.
- **Hot-reload:** just save. The script re-runs in a fresh environment, so avoid
  relying on state surviving a reload mid-Play.

## 16. Networking: `net.*`, `synced`, `onRpc`

Multiplayer in Floptle is **server-authoritative**: the host simulates the
truth, clients receive smoothed snapshots, and clients send *intents* (RPCs),
never state — so cheating means asking the server nicely. Making a node
multiplayer takes two steps, no rewrite:

1. Give it the **Networked** component (Inspector → ➕ Add Component →
   Networking), or from code: mark what syncs in its settings.
2. Declare which script vars sync with a top-level `replicated` table, and
   read/write them through `synced`:

```lua
-- door.lua — a fully networked, late-joiner-correct door in ten lines
replicated = { open = false }

onRpc = {}
function onRpc.use(args, sender)          -- a client walked up and sent net.rpc("use")
  if net.isServer() then synced.open = not synced.open end
end

function update(node, dt)                 -- cosmetic: everyone eases toward the truth
  local target = synced.open and 1.6 or 0.0
  node.y = node.y + (target - node.y) * math.min(1, dt * 6)
end
```

| Call | What it does |
|---|---|
| `net.host{ maxPlayers = 16, port = 7777, relay = "addr" }` | become the authoritative host — `relay` = get a LOBBY CODE through a rendezvous relay (nobody port-forwards); `port` = direct UDP (QUIC); neither = the in-editor harness |
| `net.host{ interest = 150, interestBudget = 16384 }` | **interest management** — each client is told about its own neighbourhood (metres) within a per-client byte budget, instead of everything. Absent = broadcast to everyone, which is cheaper below a few dozen players. Tick ⬦ *always relevant* on a node's Networked component to exempt it (the match clock, the objective, the boss) |
| `net.lobbyCode()` | the five letters friends type in, on a relay host — so your own lobby screen can show them. **Poll it**: `nil` until the relay answers (a round trip after `net.host`), and `nil` for good on a client or a direct/LAN host, where there is no code and joiners use the address |
| `net.join(addr)` | join a session (`"relay://relayaddr/CODE"` = by lobby code; `"quic://host:port"` = a server directly; `"local://"` = the in-editor test harness). **Does not block** — see `net.joinState()` |
| `net.joinState()` | `"offline"` / `"connecting"` / `"joined"` / `"refused"`, plus the reason as a second return when refused. **Wait on this, not on `net.role()`** — joining doesn't block, so role reads `"client"` from the frame you called `net.join`, whether or not that code matched any lobby |
| `net.leave()` | end the session |
| `net.role()` / `net.isServer()` / `net.isClient()` | `"offline" \| "server" \| "client"` |
| `net.peers()` / `net.ping(peer)` | connected peer ids · round-trip ms |
| `net.rpc(name, args, {to=peer, withInput=true})` | remote call — server→clients, or client→server; `withInput` stamps the tick you were seeing (for `net.rewind`) |
| `net.on(event, fn)` | `"playerJoined"/"playerLeft"` (peer id), `"connected"`, `"disconnected"` |
| `net.spawn(path, {x,y,z,owner})` | SERVER: spawn a scene's first node, replicated everywhere |
| `net.despawn(node)` | SERVER: remove it everywhere |
| `net.rewind(peer, fn)` | SERVER: run `fn` against the world as `peer` perceived it (lag compensation) |
| `net.isMine(node)` | is this node under MY control here? (cameras/HUDs pick the local player; pair with `findScripts`) |

For a **fighting game** — where the whole game is reading your opponent's exact
state this frame — the mode above is the wrong shape. See
[§16b, rollback netcode](#16b-rollback-netcode-snapshot-restore--netrandom).

**`synced` rules.** Values can be numbers, booleans, strings, and tables
(nested up to 4 levels, ≤ 1 KB encoded per var — an oversized write is dropped
whole with a Console warning, never truncated). Only the **server's** writes
replicate; writing on a client warns and gets overwritten. Late joiners receive
the current values automatically.

**RPC handlers** live in an `onRpc` table: `function onRpc.use(args, sender)`.
`sender` is the *verified* peer id (`0` = the server) — clients can't spoof it.
Args follow the same size/type rules as `synced`.

> **Test it without a second machine:** press Play, then the **🌐** toolbar
> button → *Host + join a local client*. A hidden ghost client joins over a
> simulated link — **cyan ghost spheres** show where *it* believes every
> networked node is. Drag the latency/loss sliders and watch the ghosts lag
> and stutter exactly as a real remote player would.

> **Play over a real network:** both machines open THIS project and press
> Play. One hosts (🌐 → *Host on LAN*, or `net.host{ port = 7777 }`), the
> others join (`quic://<host's-LAN-ip>:7777`). The link is QUIC — encrypted,
> zero-config (the trust model of a Minecraft server; verified identity comes
> with the relay). Player slots: **scene-authored Predicted nodes, in node
> order — #1 is the HOST's, #2 the first joiner's, #3 the second's**, and so
> on. Duplicate your character node to add a slot, and every camera/HUD picks
> its own player via `net.isMine` (the stock camera already does).

### Per-player avatars: spawn one on join

The scalable shape — no authored slot per player. The server spawns an avatar
scene for each joiner; the engine registers its physics body live, the
joiner's machine binds **prediction** to it (instant response at any latency),
everyone else interpolates it, and it despawns automatically when its player
disconnects:

```lua
-- player_spawner.lua — attach to any always-present node (the Map)
function start(node)
  net.on("playerJoined", function(peer)
    if net.isServer() then
      net.spawn("scenes/player.ron", { x = peer * 2, y = 2.5, z = 8, owner = peer })
    end
  end)
end
```

`scenes/player.ron` is a one-node scene: a capsule with a RigidBody, your
controller scripts, and a Networked component set to *Predicted* (see the
stock `player.ron`). The scene's own Predicted node (if any) stays the host's
avatar. Current limits: a spawned scene contributes its FIRST node only (no
child hierarchies yet), and spawns are dynamic bodies — not static geometry.

### Lobby codes: play without port-forwarding

Run the open relay anywhere both machines can reach (`floptle-relay`, one
binary, default port 7788 — or use a managed one), then:

- **Host:** 🌐 → *Host via relay* (or `net.host{ relay = "relay.host:7788" }`)
  → you get a five-letter **lobby code**.
- **Friends:** 🌐 → Join with `relay://relay.host:7788/CODE`
  (or `net.join("relay://…/CODE")`).

Show the code on your own lobby screen rather than sending players to the 🌐
panel — `net.lobbyCode()` returns it:

```lua
function update(node, dt)
  find("CodeLabel").text = net.lobbyCode() or "getting a code…"
end
```

Poll it rather than reading it once: the relay has to answer first, so it is
`nil` for a round trip after `net.host`. It stays `nil` on a client and on a
direct/LAN host — there is no code in either case — and it clears the moment a
session ends or a host attempt fails, so five stale letters can never sit on
screen looking live.

**Joining does not block, and the code is usually wrong.** `net.join` returns
immediately and `net.role()` reads `"client"` from that frame — before the relay
has said anything. A game that trusts role congratulates a player on joining a
lobby that was never there. Wait on `net.joinState()` instead:

```lua
function update(node, dt)
  local state, why = net.joinState()
  if state == "joined" then
    scene.load("arena")
  elseif state == "refused" then
    find("Error").text = why          -- "no lobby QK7RM", in the relay's words
  end
end
```

Mistyping the code is the most common thing that will ever go wrong in an online
session. `"refused"` is the relay actively saying no; a relay that is switched
off entirely never answers at all, and stays `"connecting"` — so give that case a
timeout of your own.

The relay is dumb on purpose: lobbies, peer ids, forwarding — it never reads
game state, and a session through it is byte-identical to a direct one. The
lobby dies when its host leaves. Self-host it forever, no strings — the
managed convenience (always-on relays near your players) is what Floptle
Cloud sells.

**Prediction** (*🌐 → Test as remote player*): give your character's node a
Networked component with mode **Predicted (owner)** and it responds instantly
at any latency — the engine records your inputs, the server re-runs the same
script with them, and divergences rewind-replay invisibly. One thing to know:
**in a session, a predicted node's `update` runs on the gameplay tick** (60 Hz,
constant `dt`) instead of per frame, so the client and server integrate your
controller identically. Your script doesn't change — but movement code belongs
in `fixedUpdate` anyway, and cameras (per-frame `update`) belong on a separate,
non-networked node.

**Which scripts run where.** On a client, a node whose **transform/physics**
the server owns is fully snapshot-driven — its scripts don't run there (its
state arrives over the wire). A Networked node that only syncs script **vars**
runs its scripts everywhere: that's the door above — `update` eases toward
`synced.open` on every machine, and the authoritative flip guards with
`net.isServer()`. Rule of thumb: sync the transform for things physics moves;
sync only vars for things scripts animate.

### Lag-compensated combat: `withInput` + `net.rewind`

On your screen, every *other* player is rendered a beat in the past (the
interpolation delay) — so by the time your "I swung" intent reaches the server,
the defender has moved on. Judged at server time, hits you clearly landed
whiff, and parries that were up on your screen don't count. The fix is the
genre's standard contract: **the server rewinds the world to what you saw and
judges there.**

Two pieces. The client stamps the intent with the tick it was seeing; the
server wraps its hit-check in `net.rewind`:

```lua
-- sword.lua — on the attacker (a Predicted node)
function update(node, dt)
  if net.isClient() and input.clicked(0) then
    local yaw = input.aimYaw() or node.yaw
    net.rpc("swing", { dx = math.sin(yaw), dz = math.cos(yaw) },
            { withInput = true })                 -- ← stamp what I was seeing
  end
end

onRpc = {}
function onRpc.swing(args, peer)                  -- runs on the SERVER
  net.rewind(peer, function()                     -- ← the world as PEER saw it
    local hit = raycast(node.x, node.y, node.z, args.dx, 0, args.dz, 3.0)
    if hit and hit.node then
      local combat = hit.node:getscript("combat")
      if combat and combat.synced.parrying then   -- their flag AT THAT TICK
        net.rpc("parried", { by = hit.node.id }, { to = peer })
      elseif combat then
        combat.hurt(25, peer)
      end
    end
  end)
end
```

Inside the `net.rewind` closure, **raycasts and shape queries see every
networked body where that player saw it**, and **other scripts' `synced` vars read the values from
that same tick** — so a parry window that was open on the attacker's screen
counts, even if it just closed at server time. Everything snaps back to the
present when the closure returns (it also passes through return values, so
`local hit = net.rewind(peer, function() return raycast(...) end)` works).

The fine print, so you can reason about fairness:

- `raycast` hits **physics bodies** (players, crates) as well as static
  geometry, and tells you who: `hit.node` is the body's node handle (nil for
  terrain/walls). Your own body is always excluded from your rays, and an
  optional trailing arg skips one more node: `raycast(…, max, someNode)` —
  what the orbit camera does so the character it follows never reads as a
  wall.
- Rewind depth is **clamped to ~250 ms** — a very-high-ping attacker can't
  shoot everyone else in the distant past. Beyond the clamp, their disadvantage
  is real (that's the honest tradeoff every game in the genre makes).
- `net.rewind` outside a server-side `onRpc` handler for a `withInput` rpc
  (or with the wrong peer) warns and runs the closure at server time — your
  logic still works, it's just not compensated.

---

## 16b. Rollback netcode: `snapshot`, `restore` & `net.random`

Everything in §16 is *server-authoritative*: one machine simulates, the others
watch a slightly delayed copy and predict their own avatar. That is the right
shape for almost every game — and the wrong shape for a fighting game, where
the decision you make is made against your opponent's exact state *this frame*.

**Rollback** is the third mode. Set a node's Networked component to
`Rollback (every peer)` and every machine simulates that node, every tick, from
the session's per-tick inputs. Nothing about a hit ever crosses the wire — only
inputs do — so hit resolution, hitstop and meter agree because the *simulation*
agrees. Full design: [`rollback-netcode-design.md`](rollback-netcode-design.md).

### The contract: two hooks

```lua
function snapshot()   return { hp = hp, meter = meter, frame = frame } end
function restore(s)   hp, meter, frame = s.hp, s.meter, s.frame end
```

That is the whole opt-in. When a remote input arrives that contradicts what was
predicted, the engine restores the last agreed tick and re-simulates every tick
since, with no rendering in between — and it restores your script's state
through these two hooks.

> **A script that defines neither hook is not rolled back — right for
> cosmetics, wrong for gameplay.**

Read that twice, because nothing warns you at runtime: a rolled-back match with
one un-snapshotted counter keeps playing, and the two machines simply stop
agreeing about it. If a value affects what happens, it belongs in `snapshot()`.

The engine owns the copy in both directions, so a replay can't corrupt the
snapshot it restored from. Rollback state holds numbers, strings, booleans and
nested tables; a node handle or a function is refused with a Console error,
because those cannot be meaningfully restored and silently dropping them would
produce a state that *looks* restored and isn't.

### Writing a fighter that stays in sync

| Rule | Why |
|---|---|
| Count **frames**, not seconds | `heldSecs` reads 0 on rollback-driven slots (the wire carries actions, not durations). Integer frame counts are exact and re-simulate identically. |
| Build hurtboxes from `node.tickPos`, never `node.x` | Between ticks the transform holds the *interpolated render pose*. Reading it in `fixedUpdate` is a frame-rate-dependent read that no replay can reproduce. |
| Move the body with `node.tickX/tickY/tickZ` or velocity, never `node.x = node.x + d` | Same reason, from the other side: that teleports the body onto its **visual** position — the model slides and the hurtbox doesn't. |
| Use `net.random()`, never an unseeded `rng()` | An unseeded roll comes from the clock. Two peers draw different numbers and the match quietly forks in two. |
| Put projectiles in rollback **state**, not in `spawn()` | A spawned prefab isn't part of the rollback state and one-shot spawns are suppressed during a replay. A fireball that must exist on both machines is data in your controller's snapshot, rendered by the controller. |
| Turn on **pushbox only** on the RigidBody | The contact solver is the part least likely to agree bit-for-bit between two machines. With it on, the body integrates its velocity and nothing else — your script owns gravity, the floor and pushout, which is how the genre works anyway. |

### The API

| Call | What it does |
|---|---|
| `net.random()` / `net.random(n)` / `net.random(a, b)` | deterministic RNG: `[0,1)`, `1..n`, `a..b`. Drawn from (match seed, tick, draw index), so every peer rolls the same numbers **and a re-simulated tick rolls them again** |
| `net.replaying()` | true while the engine is re-simulating. For cosmetics it can't see (a material poke, a UI label) — **never** branch simulation on it, that IS a desync |
| `net.rollbackDepth()` / `net.rollbackMax()` / `net.rollbackAverage()` | ticks re-simulated by the last correction · the worst so far · the mean per correction |
| `net.mispredictRate()` | 0..1 — the fraction of ticks that had to guess |
| `net.inputDelay()` | the session's fixed input delay, in ticks |
| `net.stalled()` | the sim is waiting for input rather than guessing further — show your own "connection trouble" banner off this |
| `net.on("desync", fn)` | the peers' checksums disagreed. From here the two machines are playing different matches; end the set honestly rather than play it out |

The engine suppresses one-shot side effects during a re-simulation —
`spawnEffect`, `audio.play`, prefab `spawn()`/`destroy()`, `net.rpc`, console
output. The honest consequence: a correction can *eat* a cosmetic (the spark
that only exists on the corrected timeline never fires) or *orphan* one (the
spark fired for a hit that turned out not to happen). Every rollback game lives
with this; at the depth cap it reads as network crackle, not wrongness.

### What the engine does not promise

- **Same build, same platform:** determinism is guaranteed for the profile
  above.
- **Across platforms** (x64 ↔ Apple Silicon): expected, not proven. IEEE
  add/mul/div/sqrt are bit-exact everywhere; the risk is `sin`/`cos`/`pow`/
  `atan2`, which may differ in the last bit between platform math libraries. A
  fighter's simulation path typically avoids them.
- **Bodies in solver-resolved contact:** out of scope — that is what
  `pushboxOnly` is for.

Which is exactly why checksums are **mandatory and always on**: every 30
confirmed ticks each peer hashes its state and the host compares. A mismatch is
loud — Console error naming the tick, red in the 🌐 panel, and
`net.on("desync")`. A rollback implementation without them doesn't fail
loudly; it plays a subtly different match on each screen until someone notices
the health bars disagree.

### Frame-stepping backwards

While paused, **⏮ Back (Shift+F3)** puts the simulation back one gameplay tick.
A simulation isn't invertible, so this reads the rollback state ring rather than
re-deriving anything: it needs a rollback session running and reaches back about
a fifth of a second. Stepping back and forward again lands on exactly the state
that was there — it's a scrubber, not an undo.

---

## 17. Scenes: `scene.load` & the entry scene

A game is usually more than one scene — a menu, a lobby, arenas, levels. Two
pieces make that work:

**The entry scene** (Edit ⏵ Project Settings ⏵ Game) is the scene a build
boots into. The editor opens it on project load too, so what you see is what
ships. It's saved in `project.ron` as `entry_scene`.

**`scene.load(name)`** switches scenes from code:

```lua
function update(node, dt)
    if input.pressed("return") then
        scene.load("arena")            -- scenes/arena.ron
    end
end
```

- Accepts a name (`"arena"`), a scenes-relative path (`"arenas/desert"`), or a
  project-relative path (`"scenes/arena.ron"`).
- The switch happens at the next **frame boundary**, never mid-frame under the
  scripts that asked for it. The world swaps to the new scene; physics,
  animators, particles, and audio rebuild against it; every script's `start`
  re-fires — exactly like the scene booting fresh.
- In the editor, Stop still restores **the scene you were editing** — a
  mid-play transition never touches your open file.
- `scene.current()` is the running scene's name; `scene.list()` enumerates
  every scene in the project (names `scene.load` accepts).

### Multiplayer

Only the **server** switches scenes. When the host's script calls
`scene.load`, the engine announces the switch to every client; each client
loads the same scene from its own project files and re-registers its networked
nodes — automatically, no client code needed. A **late joiner** is put into
the session's current scene by the welcome handshake (even if it had a
different scene open).

A joined client calling `scene.load` gets a Console warning and no switch —
if a player action should change the scene, send the server an RPC
(`net.send`) and let the server's script decide:

```lua
-- client
net.send("requestNextMap")

-- server
onRpc("requestNextMap", function(sender)
    if isAdmin(sender) then scene.load("arena2") end
end)
```

### Additive loads: `{ additive = true }`

A plain `scene.load` replaces the world. **Additive** layers a scene on top of
the running one — nothing is torn down, no script restarts, and the new nodes
join the live physics sim the way a `spawn(...)`ed prefab does:

```lua
scene.load("rooms/armoury", { additive = true })   -- layer it in
scene.unload("rooms/armoury")                      -- and take it away again
```

This is how you stream a level in pieces, bring in a UI overlay without losing
the world behind it, or keep a hub scene resident while a mission loads.

- An additive scene brings **nodes only** — no second sun, skybox or
  post-processing chain. A world has one environment, and the base scene owns
  it. Unless you hand it over — see `environment` below.
- `scene.unload(name)` removes exactly what the matching `load` brought, plus
  anything you parented under it (a projectile fired inside a room leaves with
  the room rather than becoming a child of nothing). The scene you opened is
  never a candidate — you cannot unload the world out from under yourself.
- Additive loads and unloads are **local**, so a client may do them in a
  session. Only a full swap is the server's alone.

#### `{ environment = true }` — letting the layer own the look

```lua
scene.load("weather/storm", { additive = true, environment = true })
```

The layer takes the world's environment over for as long as it is loaded: its
scene-level `lighting` block (sun, shadows and **all** of the fog) replaces the
base scene's, and its Skybox and PostProcess nodes replace the base scene's
too. `scene.unload` gives every bit of it back.

This exists because "nodes only" has one sharp edge. A Skybox *is* a node, so a
layer carrying one does not fail — it quietly becomes the world's **second**
skybox, and the renderer resolves both with a first-match query. Which one you
get is then spawn order, which is the "the additive scene broke my lighting"
failure the nodes-only rule was written to prevent. The option makes the
handover explicit instead of leaving it to a race.

- The base scene's environment nodes are **disabled, not destroyed** — they come
  back on `unload` wearing exactly the values they were authored with. (A
  disabled Skybox or PostProcess node is now skipped by the renderer generally,
  which is what the Inspector's checkbox always implied.)
- Load a second environment layer over the first and the second wins; unloading
  it returns the **base scene's**, not the one it displaced. There is one
  environment and one loan on it.
- A full `scene.load` voids the loan — the world it applied to is gone.
- It does nothing without `additive`; a swap already brings its own.

**It does not carry map or paint sidecars.** Those are keyed by scene name and
belong to the base scene, so a layer whose geometry is Map Mesh nodes arrives
empty however its environment is set. Layer *look*, not blockout.
- Several in one frame is fine and they all happen, in order. A full
  `scene.load` in the same frame ends the queue: everything behind it named a
  world that is about to stop existing.

### `node.persistent` — surviving the swap

```lua
node.persistent = true       -- this node, and everything under it, outlives a swap
```

A persistent node keeps its **entity**, its components, its physics body *and
its running script*. `start` does not re-fire, because the node never stopped
existing — the state in your script's locals is still there on the other side.
The DontDestroyOnLoad equivalent, for a HUD, a music player, a party, a
save-game manager.

It's a subtree rule: marking a folder carries everything under it. And it's a
**runtime** flag — set it from a script, not in a scene file; a node is only
persistent relative to a swap that happens while the game runs.

Two edges worth knowing:

- If a survivor was parented to a node that did *not* survive, it is re-rooted
  and keeps its world pose — where the player last saw it.
- If a survivor carried a Lighting/Skybox/PostProcess node, the incoming
  scene's copy wins. The scene you loaded owns the environment.

### `scene.onLoaded` — the loading-screen hook

```lua
function start(node)
    node.persistent = true                    -- outlive the load you're covering
    scene.onLoaded(function(name, additive)
        if not additive then hide(node) end   -- the new world is whole
    end)
end
```

The callback fires **after** the world is whole — a loading screen's job is to
go away once the thing it was covering exists, so being told any earlier would
be a lie. It receives the scene's name and whether it arrived additively.

A subscription dies with the script that made it, which is why the example
marks the node persistent first: something has to outlive the load to be told
about it. (For an additive load the loader survives by definition, so no
marking is needed.)

State that must survive a scene change (scores, inventory) has two homes now:
a **persistent node's script**, or — in multiplayer — the server's manager
script via an RPC/`synced` pattern. Ordinary node state still does not survive;
the old scene's nodes are gone.

---

## 18. Layers & tags

Two lightweight ways to group nodes — **layers** for physics + query filtering
(fast bitmasks under the hood), **tags** for identity checks and lookups.

### Layers

Define up to 32 named layers in **Project Settings → Layers** and pick a node's
layer at the top of the Inspector (every node starts on `Default`). Layers are
referenced **by name** everywhere — scene files, scripts, the settings matrix —
so reordering the project's list never silently re-layers a scene, and an
unknown name (a layer you removed) falls back to `Default` with a Console
warning at Play.

The **collision matrix** in Project Settings decides which layers collide:
uncheck `Ghosts × Walls` and every `Ghosts` rigidbody falls straight through
`Walls` colliders. Everything collides by default; the file only stores the
exceptions.

```lua
log(node.layer)             -- "Default" until you set one
node.layer = "Ghosts"       -- move it (a dynamic body re-layers live)
node.layer = "Ghots"        -- ERROR listing the project's layers — typos never
                            -- silently do nothing
```

Rays filter with the same names — see the `raycast` options table in
[§5](#5-input--keyboard--mouse):

```lua
local h = raycast(x, y, z, dx, dy, dz, max, { layers = { "Ground", "Walls" } })
```

### Tags

Tags are free-form strings on any node — add them in the Inspector (the `tags`
chips under the name) or at runtime. A node can carry any number of them.

```lua
node:addTag("burning")            -- duplicates are ignored
node:removeTag("burning")         -- no-op when absent
if node:hasTag("enemy") then end  -- the classic raycast hit filter
node.tags                         -- the full list (assign an array to replace)

for _, n in ipairs(findTagged("checkpoint")) do
  gizmo.sphere(n.x, n.y, n.z, 1.0)
end
```

The classic combo — a melee swing that only counts enemies:

```lua
local hit = raycast(node.x, node.y, node.z, fx, fy, fz, params.reach)
if hit and hit.node and hit.node:hasTag("enemy") then
  local hp = hit.node:getscript("health")
  if hp then hp.damage(params.power) end
end
```

Rules of thumb: a **layer** answers *"what can touch / see what?"* (it changes
physics), a **tag** answers *"what is this thing?"* (it never does). Both save
with the scene, copy/paste with nodes, and ride along when a networked spawn
replicates.

---

## 19. Vectors & math: `vec3`, `vec2`, `distance`

Real vector **values** with operators — not just x/y/z triplets:

```lua
local dir = (target.pos - node.pos):normalized()
node.pos = node.pos + dir * params.speed * dt
```

| | |
|---|---|
| `vec3(x, y, z)` / `vec3(s)` / `vec3()` | make one (splat / zero); `vec3(other)` copies |
| `a + b`, `a - b`, `v * 2`, `v / 2`, `-v`, `a == b` | operators |
| `v:length()`, `v:lengthSquared()`, `v:normalized()` | measure / unit |
| `a:dot(b)`, `a:cross(b)`, `a:lerp(b, t)`, `a:distance(b)` | the classics |
| `v:flatten(up)` | project onto the plane ⟂ `up`, renormalised — ["on any planet"](#32-on-the-ground-on-any-planet--flattenup) |
| `v:withX(n)`, `v:withY(n)`, `v:withZ(n)` | the same vector with one component replaced |
| `v:rotatedY(rad)`, `v:rotatedAround(axis, rad)` | spun about +Y, or about any axis |
| `v:towards(other, maxDelta)` | step toward, never overshooting |
| `v:angleTo(other)` | the unsigned angle between two directions (0, never NaN) |
| `vec2(x, y)` | the 2D version (UI/screen math; same surface, no cross) |
| `node.pos` | the node's position **as** a vec3 — read/write |

`distance(a, b)` is a global that takes vectors, plain `{x=, y=, z=}` tables,
or **node handles** — `distance(node, player)` just works. There's also a raw
form: `distance(x1,y1,z1, x2,y2,z2)`.

Everything that *accepts* a vector accepts anything with numeric `x/y/z`
fields — vectors, tables, nodes — so there's never a conversion dance.

### The node's own vectors

| | |
|---|---|
| `node.pos` | position (read/write) |
| `node.vel` | the body's velocity (read/write) — one write, not three |
| `node.up` | the body's up (−gravity): Y on flat ground, **radial** on a planet |
| `node.forward` | facing, from the rotation (−Z forward, matching the camera) |
| `node.right` | the node's +X axis |
| `node.size` | the whole scale as a vec3 (`node.scale` stays the uniform one, and takes a vec3 too) |

```lua
-- a jump in whatever direction "up" means where the player is standing
if node.grounded and input.action("jump") then
  node.vel = node.vel + node.up * params.jump
end

-- camera-relative movement, without a line of trigonometry
local mx, my = input.axis2("move")
node.pos = node.pos + (node.right * mx + node.forward * my) * params.walk * dt
```

The scalar spellings (`node.vx`, `node.up_x`, `node.scale_x`) still work and
always will — they're just not what the docs teach any more.

### `math.*` — the arithmetic you were writing by hand

| | |
|---|---|
| `math.clamp(x, lo, hi)` · `math.saturate(x)` · `math.sign(x)` | the everyday three |
| `math.round(x [, step])` | nearest whole, or nearest multiple (`round(x, 0.25)` snaps to quarters) |
| `math.lerp(a, b, t)` · `math.mix(a, b, t)` | blend — **unclamped** / clamped |
| `math.inverseLerp(a, b, x)` · `math.remap(x, a, b, c, d)` | the inverse, and range→range |
| `math.smoothstep(a, b, x)` | 0..1 with eased ends |
| `math.approach(cur, target, maxDelta)` | move toward without **ever overshooting** — pass `rate * dt` |
| `math.wrapAngle(a)` · `math.deltaAngle(a, b)` | fold into (−π, π] · the **short** way round |
| `math.approachAngle(cur, target, maxDelta)` | "turn to face", correct across the seam |
| `math.pingPong(t, len)` | 0 → len → 0, forever |
| `ease(a, b, rate, dt)` | frame-rate-independent exponential ease — numbers **or** vectors |
| `smoothDamp(cur, target, vel, time, dt)` | → `value, vel` — a critically-damped spring, with momentum |

```lua
-- a turret that turns the short way and never overshoots
node.yaw = math.approachAngle(node.yaw, wanted, params.turn_rate * dt)
-- fade something out with distance
local alpha = math.remap(distance(node, player), 5, 25, 1, 0)
```

### `table.*` — lists without the bookkeeping loop

| | |
|---|---|
| `table.map(list, fn)` · `table.filter(list, fn)` · `table.reverse(list)` | new lists (never mutates) |
| `table.find(list, fn)` | → `value, index` — takes a **predicate** |
| `table.indexOf(list, v)` · `table.count(t [, fn])` · `table.sum(list [, fn])` | look up / tally |
| `table.keys(t)` | keys as a **sorted** list (raw `pairs` order isn't reproducible) |
| `table.copy(t)` · `table.extend(dst, src)` | shallow copy · append in place |

```lua
local ready = table.filter(ships, function(s) return s.fuel > 0 end)
local total = table.sum(ready, function(s) return s.fuel end)
local names = table.concat(table.map(ready, function(s) return s.name end), ", ")
```

---

### Seeded randomness & noise

For gameplay that must **reproduce** — loot rolls, procedural scatter, anything a
server might replay — use the engine's deterministic stream instead of
`math.random`:

```lua
local r = rng(42)                 -- same seed = same sequence, every machine
local roll = r:next()             -- [0, 1)
local dmg  = r:range(4, 9)        -- [4, 9)
local n    = r:int(1, 3)          -- 1, 2 or 3
local item = r:pick({"sword", "bow", "wand"})

-- Terrain-style variation (identical numbers to the Rust generators):
local h = math.fbm(x * 0.05, 0, z * 0.05)      -- ≈ -1..1, 4 octaves
local v = math.noise(x, y, z, 7)               -- one octave, seed 7
```

## 20. Collision & trigger events

Define these hooks in any script on a node and the engine calls them when the
node's body touches something — per gameplay tick, right after physics:

```lua
function onCollisionEnter(node, other, hit)  -- the touch STARTED this tick
end
function onCollisionStay(node, other, hit)   -- every tick while it lasts
end
function onCollisionExit(node, other, hit)   -- the pair separated (hit = last contact)
end
```

- `other` is the other node's handle — `other.name`, `other:hasTag("enemy")`,
  `other:getscript("health")` all work.
- `hit` is `{ x, y, z, nx, ny, nz }`: the world contact point and the unit
  normal out of the surface that was hit.
- Fires for body-vs-collider **and body-vs-body** (two rigidbodies detect each
  other even though the solver doesn't push them apart).
- The events fire on **both** nodes' scripts, and the collision matrix
  (Project Settings → Layers) gates them: pairs that don't collide don't event.
- A body resting on the floor reports `onCollisionStay` against the floor node
  every tick — gate on tags/names rather than assuming silence.

### Triggers

Tick **trigger** on a node's Collider component and it stops blocking: bodies
(and raycasts) pass straight through, but overlap fires the trigger hooks —
portals, pickup zones, checkpoints, kill planes:

```lua
function onTriggerEnter(node, other, hit) end
function onTriggerStay(node, other, hit) end
function onTriggerExit(node, other, hit) end
```

Triggers work on **rigidbody nodes too** — the trigger checkbox sits on the
Rigidbody component there, and it turns the *body* into a sensor: it never
blocks or gets blocked (and rays skip it), but overlap fires the hooks on both
nodes. A **Kinematic + trigger** rigidbody is the moving pickup / sweeping
damage zone: scripts move it, players pass through it, `onTriggerEnter` fires.
A **Dynamic + trigger** body still falls — it drops straight through solid
geometry (firing trigger events against everything it crosses), so pair
triggers with Kinematic or gravity-off for things that should stay put.

The full portal — **one script, any number of portals**, each with its own
destination via a [string param](#6-globals-params-time-dt-log):

```lua
-- portal.lua — attach to a Collidable node with "trigger" ticked
defaults = { destination = "hub" }

function onTriggerEnter(node, other, hit)
  if other:hasTag("player") then
    scene.load(params.destination)
  end
end
```

### When events fire (and don't)

Events are produced where physics runs: offline everywhere, on the **server**
in multiplayer, and on a predicted node's owning client. Prediction **replays
never re-fire events** (corrections can't double-trigger a pickup). Handlers
run outside the normal `update` pass — their `node` writes apply immediately,
but `params` writes are frame-local there (persist state in script variables
or `synced` instead).

## 21. Prefabs: `spawn` & `destroy`

A **prefab** is a reusable node (with its whole child subtree) saved as an
asset. Make one by **dragging a node from the Hierarchy into the Assets
panel** (drop on a folder to aim; it lands in `prefabs/` otherwise), or
right-click the node → **◇ Save as Prefab**. Place instances by dragging the
prefab into the viewport, dropping it on a Hierarchy row (spawns as that
node's child), or right-click → **Add to scene**.

At runtime, scripts spawn and remove them:

```lua
-- spawn(prefab [, pos [, fn]]) — the callback gets the new root's handle
spawn("bullet")                                   -- at its authored spot
spawn("bullet", node.pos + dir * 1.5)             -- at a position
spawn("bullet", node.pos + dir * 1.5, function(b) -- ...and configure it
  b:getcomponent("RigidBody").vx = dir.x * 40
  b:getcomponent("RigidBody").vz = dir.z * 40
end)

destroy(other)      -- remove a node (and all its children)
node:destroy()      -- same thing, method form (self-destruct a pickup)
```

| Call | What it does |
|---|---|
| `spawn(prefab)` | spawn an instance — `"bullet"` finds `prefabs/bullet.prefab.ron`; subfolders (`"weapons/sword"`) and full paths work too |
| `spawn(prefab, pos)` | ...with its first root placed at `pos` (a vec3/table/node — sibling roots keep their relative offsets) |
| `spawn(prefab, pos, fn)` | ...then call `fn(root)` with the new node's handle, same frame — velocities, params, tags, whatever |
| `spawn(prefab, pos, fn, parentNode)` | ...spawned as a CHILD of `parentNode`, still landing at the world `pos` (converted into the parent's frame). How a blueprint spawner assembles parts under a vessel's assembly root — follow with `assembly.rebuild(parentNode)` |
| `destroy(node)` / `node:destroy()` | queue the node + its whole subtree for removal (applied after the pass, so the handle stays readable through the current call) |

The spawned node is complete immediately: rigidbodies simulate (all three
[body modes](#4-node--the-physics-body)), its scripts fire `start` next pass,
animators/particles/audio wire themselves. Everything is undo-free play-state
— Stop discards it like any other play change.

**Multiplayer**: `spawn()`/`destroy()` are LOCAL. For replicated objects, the
server calls `net.spawn("bullet", {x=…, y=…, z=…})` — it accepts prefab names
now (single-node prefabs; replication is per-node) — and `net.despawn(node)`,
which broadcast to every client. `destroy()` on the server also routes
replicated nodes through the session automatically; on a client it refuses
(server authority).

**Gotcha**: a spawned prop that should be *solid* needs a Rigidbody in
**Static** mode (a plain Collidable marker only bakes at Play start).

## 22. Terrain: `terrain.sculpt`, `dig` & queries

Terrain is **runtime-editable**: the same sparse SDF field the editor's Sculpt
brush writes is exposed to scripts, and an edit lands the **same tick** — the
drawn surface, the physics collider, and the sun-shadow field all update
together, so the tick that dug the hole also falls into it.

All coordinates are **world space**. Edits target the nearest terrain surface
to the given point; a call far from every terrain is a safe no-op.

```lua
-- Dig where the player aims (LMB), raise with RMB.
function update(node, dt)
  local yaw, pitch = input.aimYaw(), input.aimPitch()
  local cp = math.cos(pitch)
  local dx, dy, dz = -math.sin(yaw) * cp, math.sin(pitch), -math.cos(yaw) * cp
  local h = raycast(node.x, node.y + 1.0, node.z, dx, dy, dz, 30, node)
  if h then
    if input.button(0) then terrain.dig(h.x, h.y, h.z, 2.5, 0.8) end
    if input.button(1) then terrain.sculpt(h.x, h.y, h.z, 2.5, 0.8, "raise") end
  end
end
```

| call | effect |
|---|---|
| `terrain.sculpt(x,y,z, radius [, strength [, mode]])` | sculpt: mode `"raise"` (default), `"lower"`/`"dig"`, `"smooth"`, `"flatten"`; strength 0–1 |
| `terrain.dig(x,y,z, radius [, strength])` | sugar for `sculpt(..., "lower")` |
| `terrain.paint(x,y,z, radius, r,g,b [, strength])` | recolor the surface (0–1 colors) |
| `terrain.paintTexture(x,y,z, radius, slot)` | paint a palette texture slot (1-based; 0 clears) |
| `terrain.query(x,y,z)` → `d` | signed distance to the nearest terrain surface (negative = inside rock); `nil` with no terrain |
| `terrain.slotAt(x,y,z)` → `slot` | the texture-palette slot at a point — *what the rock is made of*; `nil` where untextured |
| `terrain.height(x, z)` → `y` | world Y of the highest surface under (x,z); `nil` if none |
| `terrain.yields()` → `list` | the reports for edits that have **landed** since the last call (drained) |

### What a dig removed

`sculpt` and `dig` return an **id**, not a result — the edit is queued and
applied after the script pass, so nothing has been dug yet at the moment they
return. The measured report arrives through `terrain.yields()` on a later frame,
carrying that id:

```lua
local pending = {}

function update(node, dt)
  if input.pressed("mouse1") then
    local h = raycast(cam, dir, 50)
    if h then pending[terrain.dig(h.x, h.y, h.z, 2.0)] = true end
  end
  for _, y in ipairs(terrain.yields()) do
    if pending[y.id] then
      pending[y.id] = nil
      for slot, volume in pairs(y.slots) do
        inventory.add(ORE[slot], volume)       -- what it was, and how much
      end
    end
  end
end
```

Each report is `{ id, removed, added, untextured, slots = { [slot] = volume } }`,
in **world cubic units**. `removed == untextured + sum(slots)`, so a caller can
check its own arithmetic, and the volumes are additive: sum them over a shaft and
you get the volume that actually left the field — a careful shaft and a sloppy
cavern differ by the truth rather than by the number of dabs. An edit that moved
nothing reports zero rather than not reporting, so "I dug air" is
distinguishable from "the report hasn't arrived".

Notes:

* Edits during Play are **simulation state**: Stop restores the authored
  terrain exactly, like every other play-mode change.
* Radius is clamped (≤ 64) and edits cap at 64 per frame — a runaway loop
  warns instead of freezing the frame.
* **Multiplayer**: edits apply on the machine that runs them, and the ops are
  deterministic — the same call produces the same field everywhere. Until
  replicated terrain ships, run edits **server-side** and mirror them with an
  RPC that repeats the call on clients (`net.rpc("dig", {x=…}, …)` →
  `onRpc.dig` calls `terrain.dig` locally). The local test harness (ghost
  client) doesn't support terrain edits yet and will say so in the Console.

---

## 22a. Water: volumes, buoyancy & `water.*`

A **Water Volume** node is a body of water the engine simulates: things float
in it, are dragged by it, and the world goes murky when the camera is under it.
Add one from the Inspector's type menu (`≈ Water Volume`).

Two shapes:

- **Sea** — a sphere about the node. A planet's ocean: "up" is different at
  every point on it, which is why this is not a very large flat pool.
- **Pool** — an oriented box. A lake, a tank, a flooded room. Rotate the node
  and the surface tilts with it. Its **sides are walls**: standing beside a pool
  at the same height as its water is not standing in it.

### What the engine does

**Buoyancy** is Archimedes, per shape. Whether a thing floats is its own
density against the water's — mass over volume, both of which the engine
already knows — so a wooden crate bobs and a lead ball sinks with no flags to
set. On an assembly the push is applied at **each part's own position**, so a
hull that lands flat floats and the same hull nose-down sinks its nose and
rights itself. A single force at the centre of mass would give you a craft that
bobs but never rights itself.

**Drag is quadratic**, which is what makes a gentle touchdown float and a
60 m/s belly-flop stop hard without either being a special case.

**Underwater** replaces the scene's fog with the volume's tint and visibility.
Because it goes through the one fog channel every draw path already reads,
meshes, terrain, SDF matter and particles go murky *together* rather than one
of them staying crisp. It works in the editor viewport too, so tuning the tint
isn't guesswork.

**Frozen** is a state, not a second system. A frozen sea applies no buoyancy,
no drag and no underwater look; add a `Collidable` surface and it becomes
walkable ground. A script can thaw it.

### What a script does

The engine floats things. What being *wet* means — swimming, drowning, a
flooded engine, a gauge going red, the music ducking — is the game's, and all
of it is the same question with different answers:

```lua
local d = water.depthAt(node.pos)          -- metres below the surface, 0 in air
if d > 0 then
  swimming = true
end
```

```lua
-- The detailed answer, when you need more than the depth.
local w = water.at(node.pos)
if w then
  -- w.depth, w.density, w.frozen, w.node, and w.up — the direction OUT of the
  -- water, which is radial on a sea and NOT −gravity in a tilted tank.
  node.vel = node.vel + w.up * (kick * dt)
end
```

| Call | Answers |
| --- | --- |
| `water.depthAt(x, y, z)` | metres below the surface; `0` in air. Takes a vec3 or a node too. |
| `water.at(x, y, z)` | `nil` in air, else `{depth, density, frozen, node, up}`. |
| `water.isUnderwater(x, y, z)` | the yes/no, when that's all you wanted. |
| `water.setFrozen(node, on)` | freeze or thaw a volume. |
| `water.volumes()` | every water node in the scene. |

`water.depthAt` and the solver answer from the **same geometry**, so a swim
state can't disagree with the physics floating it — which is exactly what
happened when a game carried its own `seaDepth()` against a sea radius it had
to keep in step with the sphere it drew.

### Not yet

The surface is a translucent, tinted, specular volume sized to what the solver
uses. It has **no waves, no shoreline softening against terrain and no
depth-based tint from outside** — an authorable `.flsl` water surface is still
to come. Underwater is a fog/colour grade, not refraction.

## 22b. Scatter: thousands of props from a seed

`scatter.create{...}` declares a rule; the engine places and draws every
instance from it, GPU-instanced, with **no scene node anywhere in it**.

The division of labour is the point. Your generator keeps deciding *what grows
where* — it rolls the species, reads the climate, picks the palette. The engine
decides where each instance stands and draws them all.

```lua
forest = scatter.create{
  asset   = "assets/models/pine.glb",
  seed    = worldSeed,
  center  = planet.pos, radius = planet.radius,   -- a planet's surface
  perChunk = 32, chunk = 24,
  scaleMin = 0.8, scaleMax = 1.6,
  lod = {
    { asset = "assets/models/pine.glb",     distance = 60 },
    { asset = "assets/models/pine_far.glb", distance = 220 },
  },
  fade = 12,
}
```

Leave out `radius` and give `halfX`/`halfZ` instead for a flat region — a
level, an island, a lawn.

### Determinism is the design

Every instance is `hash(seed, chunk, index)` and nothing else. Three things fall
out of that, and all three are requirements rather than conveniences:

- **Walk away and back and the same trees stand in the same places.** A chunk is
  recomputed, never remembered.
- **A multiplayer session never replicates scenery.** Same seed, same chunk,
  same instances, on every machine.
- **"This one is gone" is storable.** An instance id is stable, so a removal set
  is a handful of numbers — not the position of every plant you ever saw.

### Placement and LOD

Props are dropped onto the **real surface**: the engine casts down from above
each one and settles it on whatever is actually there, taking that ground's
normal so a hillside's trees lean with the hill. A prop with no ground under it
is dropped rather than left hanging in the air over a canyon.

**Digging the ground out from under one re-settles it**, because placement was
never remembered in the first place.

LOD bands **cross-dissolve** rather than switching — the pop at a band boundary
is the thing everyone notices about scatter and nothing else about it. Past the
last band's distance an instance is culled.

### On a body that moves, give the field a `parent`

A region is pinned to the world unless you say otherwise. That is right for a
landscape and wrong for every celestial body: a planet on orbital rails leaves
its own props behind, and at a few hundred units a second it does it in seconds.

```lua
scatter.create{
  parent = "Umunquo",              -- the node the region rides
  center = planet.pos, radius = 107,
  lod = { { asset = "rock.glb", distance = 190 } },
}
```

With a `parent`, the region is expressed relative to that node and follows
whatever it does — an orbit, a parent transform, a floating-origin rebase. It
costs nothing per prop: every id, every position on the surface and every
settled ground height is already stored in the body's own frame, so the body
moving changes one transform and re-rolls nothing. The same rock stays the same
rock, and anything you harvested stays harvested.

`scatter.near` still takes and returns **world** positions — the frame is the
engine's business, not yours.

### The outermost `lod` distance is your budget

`lod` reads as a look — how far you can see rock. It is really the cost knob,
and it is the only one that squares.

That last distance sets how many chunks stay **resident**, as a square sweep
whose side grows with it, and that sweep is walked every frame with a distance
computed for every prop in it. So:

> cost ≈ (far ÷ chunk)² × perChunk, per source, per frame

Halving `far`, or doubling `chunk`, quarters it. `perChunk` is linear and is the
knob to reach for when you want the field thicker or thinner — it is the cheap
one.

Ask, rather than guess:

```lua
local field = scatter.create{ center = planet.pos, radius = 107,
                              chunk = 34, perChunk = 14,
                              lod = { { asset = "rock.glb", distance = 190 } } }

local c = scatter.cost(field)
log(("%d chunks, %d props"):format(c.chunks, c.props))   -- 121 chunks, 1694 props
```

Roughly what to expect: a walkable body wants **tens to a couple of hundred
chunks**, a few thousand props. A field big enough to matter also says so in the
Console the moment you declare it, naming the two numbers that decided it —
you should not have to go looking.

Two things the engine does so a big field degrades instead of stopping:

* Chunks arrive **nearest first**, and only a few hundred props are dropped onto
  the ground per frame. A field coming into view fills in over a few frames from
  where you stand outwards, rather than freezing the one frame it arrives on.
* On a body **smaller than your view distance**, residency saturates at the body.
  Asking to see 700 m of a 214 m planet costs exactly what asking to see 190 m
  costs.

### Harvesting

```lua
-- What is near the tool tip?
local hits = scatter.near(forest, node.pos + node.forward * 2, 2.5)
if hits[1] then
  scatter.remove(forest, hits[1].id)      -- and it stays gone
  inventory.add("wood", hits[1].scale * 4)
end
```

| Call | Does |
| --- | --- |
| `scatter.create{...}` | declare a source; returns its id |
| `scatter.near(id, point, radius)` | instances around a point, nearest first: `{id, distance, pos, scale, param}` |
| `scatter.remove(id, instanceId)` | remove one, permanently |
| `scatter.restore(id [, instanceId])` | put one back, or all of them (regrowth) |
| `scatter.removed(id)` | the ids this source has lost — **this** is what you save |
| `scatter.cost(id)` | what it asks for per frame: `{chunks, props, far, chunkSize, perChunk}` |
| `scatter.destroy(id)` | drop the whole source |

`param` is a stable per-instance 0..1 you can map to a variant or a yield. It
also rides the albedo, so one species gets a spread of shades without a material
per plant.

### Not yet

**No per-instance colliders.** You cannot walk into a scattered tree and a
raycast will not hit one — aim with `scatter.near`, which is a proximity query,
not a ray. Prototypes are **mesh assets**, not prefabs or script-built subtrees.

## 23. Saving: `save.set`, `save.get` & slots

Persistent game data — survives Play sessions, editor restarts, and ships with
exported builds. One key→value store per **slot** (its own file under `save/`).

```lua
save.set("gold", save.get("gold", 0) + 10)
save.set("checkpoint", { scene = scene.current(), x = node.x, y = node.y, z = node.z })
save.flush()                       -- checkpoint NOW (else: auto on Stop + ~5 s)

local cp = save.get("checkpoint")
if cp then scene.load(cp.scene) end

save.slot("slot2")                 -- separate profile; save.slot() reads the name
```

Values follow the `synced`-var guardrails: numbers, strings, booleans, tables up
to depth 4 and ≤ 1 KB each — no functions/userdata. A violation is a script
error, not silent data loss.

**Multiplayer**: this is *local* storage. For server-authoritative progress,
call `save.*` inside server-side paths (`net.isServer()`) and hand results to
clients via `synced` vars or RPC.

## 24. Timers: `after`, `every` & `tween`

Schedule work in **game time** — tick-driven and deterministic (timers pause
with the game, fire at the same tick on every machine, and never drift with
frame rate). Callbacks get no arguments; capture what you need as locals.

```lua
after(2, function() door.visible = false end)      -- once, in 2 s

local beeper = every(1, function()                 -- repeatedly, every 1 s
  audio.play("sounds/beep.ogg")
end)
beeper:cancel()                                    -- stop it (handles all have :cancel())

local y0 = node.y                                  -- animate: alpha eases 0 → 1
tween(0.5, function(a) node.y = y0 + a * 3 end, "smooth")
```

* `after(seconds, fn) → handle` — fire once.
* `every(seconds, fn) → handle` — first fire after one period, then anchored
  repeats (a long session doesn't drift; a stall never bursts to catch up).
* `tween(seconds, fn [, ease]) → handle` — `fn(alpha)` every tick, the final
  call landing **exactly** at `1.0`. Eases: `"linear"` (default), `"smooth"`,
  `"in"`, `"out"`.

An error inside a callback logs to the Console and kills only that timer. On a
scene switch all pending timers drop (they belonged to the old scene). In a
networked session timers advance on the global tick only — prediction replays
can't double-fire them.

## 25. Space: orbits, gravity & time-warp

Scenes with **Celestial Body** components (Add Component → ☉) put planets and
moons on exact Kepler rails: every tick the engine writes their positions from
orbital elements (stable at any warp — no integration, no drift), and each body
pulls real **µ/r² gravity** with patched-conic dominance: the deepest sphere of
influence containing you is the ONE body that pulls (moon beats planet beats
sun). The root body (empty `parent`) stays where the scene puts it.

```lua
print(space.time())                 -- seconds of celestial time (warp-scaled)
space.warp(50)                      -- rails fast-forward 50×; physics stays 1×

local moon = space.body("Pebble")   -- {name, x,y,z, vx,vy,vz, mu, radius, soi}
print(space.dominant(node.x, node.y, node.z))   -- who owns me here?
local gx, gy, gz = space.gravity(node.x, node.y, node.z)

-- The conic your ship is ON around its dominant body (HUD / map readout):
local o = space.elements(node.x, node.y, node.z, node.vx, node.vy, node.vz)
if o then print(o.body, o.periapsis, o.apoapsis, o.period) end
```

`space.elements` returns `{ body, a, e, periapsis, apoapsis, period }` —
`apoapsis`/`period` are absent on an escape trajectory; distances are from the
body **center** (subtract `radius` for altitude). Bodies should be **top-level
nodes** — rails write world positions.

```lua
-- Where a state vector will be dt seconds from now, on its two-body conic:
local px, py, pz, vx, vy, vz =
  space.propagate(rx, ry, rz, sx, sy, sz, body.mu, dt)
```

`space.propagate` is the primitive for **planning** — maneuver nodes and
patched-conic **SOI-encounter** finding are built from it (both live in the
demo's `ship_controller.lua`, not the engine). It converts the `(pos, vel)` you
give into an orbit and evaluates it at `+dt` seconds, exactly and drift-free
(elliptic OR hyperbolic). The state is in **whatever frame you pass** — to walk a
ship's future path you propagate it relative to its attractor, then add where
that attractor itself has moved (`space.bodies()` velocities are world-frame, so
each body's own conic comes from its state minus its parent's). Chain those and
you can march a trajectory across SOI changes — leave a planet, coast in the
star's frame, fall into the next planet's SOI — the whole KSP transfer picture.

**Velocity frames.** A dynamic node's `vx/vy/vz` are measured in its dominant
celestial's carried frame (the SOI you're inside moves, and you move with it) —
so pass them to `space.elements` as-is, and never subtract the dominant body's
world velocity from them. Celestial velocities from `space.bodies()`/`body()`
ARE world-frame — subtracting a parent's from a child's gives the child's
orbital motion (what the map draws). Crossing an SOI boundary re-expresses your
velocity in the new frame automatically, keeping world velocity continuous.

---

## 26. The web: `http.*` & `json.*`

Non-blocking requests to your own server, so a game can have an account, a card
list, a leaderboard or a shop. The callback runs on a later frame **on the main
thread**, so it is safe to touch nodes from it.

```lua
http.get(url [, opts], function(res) end)
http.post(url, body [, opts], function(res) end)   -- a TABLE body is sent as JSON
-- opts = { headers = {...}, timeout = 10, json = true }
-- res  = { ok, status, body, json, error }

json.encode(t)   json.decode(s)   -- decode returns nil, err rather than raising
openUrl(url)     -- open the player's own browser (the sign-in flow needs it)
```

Play only; Stop and `scene.load` cancel everything in flight; a call from
`fixedUpdate` warns, because a reply's timing can never be replayed.

**[docs/web-api.md](web-api.md) is the full page** — the `res` table in detail,
the device-code sign-in flow (`assets/scripts/web_login.lua`), the rate limits,
and the one rule that makes an account-backed game possible at all:

> **The server decides what the player owns.** The client asks; it never
> announces. Anything a client can announce, a modified client can announce.

---

## 27. The player's account: `account.*`

`http.*` is your game talking to *your* server. `account.*` is your game talking
to **Floptle's** — Foverse accounts, Fobucks, cloud saves, leaderboards and
missions, on `fopull.com`.

```lua
account.signIn()          -- returns immediately; the player approves in a browser
account.state()           -- "signedOut" | "starting" | "waiting" | "signedIn" | "failed"
account.code()            -- while waiting: { code = "WXYZ-9999", url = "…", expiresIn }
account.player()          -- when signed in: { id, name, email, tier }
account.error()  account.cancel()  account.signOut()

account.get("/wallet", function(res) end)
account.post("/games/mygame/events", { event = "boss_killed", event_id = id }, cb)
account.put("/games/mygame/saves/slot1", { data = t }, cb)
```

The engine drives the OAuth device flow in Rust, because the provider mandates
PKCE S256 and Lua has no SHA-256 — so a script asks for a **player**, never a
token. There is no `account.token()` on purpose: a shipped game's Lua is
readable, and anything a script can hold, somebody can read out of the file.

The calls take a **path**, not a URL. One host, which is what makes attaching
the player's token to it safe.

The session lives in the OS keyring and is **shared with the Floptle Hub** —
sign in once, in whichever you opened.

**[docs/web-api.md](web-api.md) § Floptle Cloud** is the full page: the mission
and wallet shapes, why the wallet is read-only, and the three answers that
surprise a first test (`event_id` is mandatory, an empty `awarded` is not always
a failure, and a mission pays nothing until it is approved).
