# Physics

Bodies, collisions, and asking the world what is in front of you.

Part of the [scripting guide](../scripting.md) · [every call, as a reference](../lua-api.md)

## Contents

- [4. `node` — the physics body](#4-node-the-physics-body)
- [20. Collision & trigger events](#20-collision-trigger-events)

---

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
[§7](assets.md#7-assets-swapping-models-materials)).

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

---

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
destination via a [string param](start.md#6-globals-params-time-dt-log):

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
