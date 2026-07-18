# Scripting in Floptle (Lua)

Game logic in Floptle is written in **Lua**. A script is a `.lua` file in your
project's `scripts/` folder. Attach it to a node and it runs every frame while the
game is playing. Scripts **hot-reload** — save the file and the running game picks
it up immediately.

> The same reference is built into the editor: open the **Scripting** tab → **§ Docs**,
> and the API shows up as autocomplete + hover hints as you type.

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
17. [Scenes: `scene.load` & the entry scene](#17-scenes-sceneload--the-entry-scene)
18. [Layers & tags](#18-layers--tags)
19. [Vectors & math: `vec3`, `vec2`, `distance`](#19-vectors--math-vec3-vec2-distance)
20. [Collision & trigger events](#20-collision--trigger-events)
21. [Prefabs: `spawn` & `destroy`](#21-prefabs-spawn--destroy)
22. [Terrain: `terrain.sculpt`, `dig` & queries](#22-terrain-terrainsculpt-dig--queries)
23. [Saving: `save.set`, `save.get` & slots](#23-saving-saveset-saveget--slots)

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

## 5. `input` — keyboard & mouse

Available while playing.

| Call | Returns |
|---|---|
| `input.key("w")` | `true` while the key is held |
| `input.pressed("space")` | `true` only on the frame it goes **down** (an edge) |
| `input.released("space")` | `true` only on the frame it goes **up** (an edge) |
| `input.axis("a", "d")` | `-1` / `0` / `1` from a negative/positive key pair |
| `input.button(1)` | mouse button held (`0` left, `1` right, `2` middle) |
| `input.clicked(1)` | mouse button pressed this frame (an edge) |
| `local dx, dy = input.mouse_delta()` | mouse movement since last frame |
| `local x, y = input.mouse()` | cursor position, pixels |
| `input.scroll()` | wheel delta this frame |
| `input.setMouseLocked(true)` | pin + hide the cursor (FPS mouselook); `false` releases. Also `input.lockMouse()` / `input.unlockMouse()` |

Key names: `a`–`z`, `0`–`9`, `space`, `enter`, `escape`, `tab`, `backspace`,
`delete`, `shift`, `ctrl`, `alt`, `,`, `.`, and arrows `left` `right` `up` `down`.

A locked cursor is genuinely pinned to the window center (hardware lock where
the OS supports it, per-frame re-centering where it doesn't) — read motion with
`input.mouse_delta()`. Stop always releases the lock.

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
| `getcomponent("UiElement")` | `visible` (1/0), `opacity`, `posX` `posY` (free position or pin offset, design units), `width` `height` (the number in the axis's sizing mode: px value, % fraction, or grow weight; `nil` on a *fit* axis — writing one makes it fixed px), `radius`, `border`, `fillR/G/B/A`, `textSize`, `textR/G/B/A`, `tintR/G/B/A`. |
| `getcomponent("UiSlider")` | `value`, `min`, `max` — on a slider (track) element. `value` is clamped to the range at draw time. |
| `getcomponent("UiLayer")` | `enabled` (1/0 — an off layer draws nothing), `z`, `designHeight`. |

Handles are `nil` when the node lacks the component — a node without an Element spec
has no `"UiElement"`, only slider tracks have `"UiSlider"`, only layers have
`"UiLayer"`.

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

### 3D lines (`draw.line`)

Scripts can draw **world-space 3D lines** — the runtime line layer behind the
solar demo's KSP-style map (orbit conics, SOI rings, markers) and any debug
overlay you like:

```lua
draw.line(a.x, a.y, a.z, b.x, b.y, b.z, 0.3, 0.85, 1.0)        -- rgb
draw.line(x1, y1, z1, x2, y2, z2, 0.5, 0.5, 0.6, 0.4)          -- + alpha
```

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
| `anim:state([layer])` / `anim:time([layer])` | what's showing / seconds in |
| `anim:finished([layer])` | a one-shot reached its end |
| `anim:isPlaying([state])` | is a state (or anything) playing |
| `anim:clips()` / `anim:layers()` | available state / layer names |

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
3. Attach **`character.lua`**.

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
- **Completion & docs** — typing suggests the engine API *and* identifiers
  from the file, with the highlighted entry's doc shown right in the popup:
  `↑`/`↓` choose, `Tab` accepts, `Esc` hides it (`Enter` is always just a
  newline — it never accepts a completion). It understands member
  access on **any variable** — `rb.fri` offers `friction`, `anim:pl` offers
  `play`, and `params.` offers this script's own `defaults` keys. Hovering an
  API name in code shows its doc, and the **§ Docs** page has a search box
  over the whole guide + API reference.

The full shortcut list lives on the tab's **§ Docs** page.

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
| `net.join(addr)` | join a session (`"relay://relayaddr/CODE"` = by lobby code; `"quic://host:port"` = a server directly; `"local://"` = the in-editor test harness) |
| `net.leave()` | end the session |
| `net.role()` / `net.isServer()` / `net.isClient()` | `"offline" \| "server" \| "client"` |
| `net.peers()` / `net.ping(peer)` | connected peer ids · round-trip ms |
| `net.rpc(name, args, {to=peer, withInput=true})` | remote call — server→clients, or client→server; `withInput` stamps the tick you were seeing (for `net.rewind`) |
| `net.on(event, fn)` | `"playerJoined"/"playerLeft"` (peer id), `"connected"`, `"disconnected"` |
| `net.spawn(path, {x,y,z,owner})` | SERVER: spawn a scene's first node, replicated everywhere |
| `net.despawn(node)` | SERVER: remove it everywhere |
| `net.rewind(peer, fn)` | SERVER: run `fn` against the world as `peer` perceived it (lag compensation) |
| `net.isMine(node)` | is this node under MY control here? (cameras/HUDs pick the local player; pair with `findScripts`) |

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

Inside the `net.rewind` closure, **raycasts see every networked body where
that player saw it**, and **other scripts' `synced` vars read the values from
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

State that must survive a scene change (scores, inventory) lives in your
scripts' hands: stash it via an RPC/`synced` pattern before switching, or keep
it on the server's manager script — node state itself does not survive (the
old scene's nodes are gone).

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
| `vec2(x, y)` | the 2D version (UI/screen math; same surface, no cross) |
| `node.pos` | the node's position **as** a vec3 — read/write |

`distance(a, b)` is a global that takes vectors, plain `{x=, y=, z=}` tables,
or **node handles** — `distance(node, player)` just works. There's also a raw
form: `distance(x1,y1,z1, x2,y2,z2)`.

Everything that *accepts* a vector accepts anything with numeric `x/y/z`
fields — vectors, tables, nodes — so there's never a conversion dance.

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
right-click the node → **⬡ Save as Prefab**. Place instances by dragging the
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
| `terrain.height(x, z)` → `y` | world Y of the highest surface under (x,z); `nil` if none |

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

Scenes with **Celestial Body** components (Add Component → 🪐) put planets and
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

**Velocity frames.** A dynamic node's `vx/vy/vz` are measured in its dominant
celestial's carried frame (the SOI you're inside moves, and you move with it) —
so pass them to `space.elements` as-is, and never subtract the dominant body's
world velocity from them. Celestial velocities from `space.bodies()`/`body()`
ARE world-frame — subtracting a parent's from a child's gives the child's
orbital motion (what the map draws). Crossing an SOI boundary re-expresses your
velocity in the new frame automatically, keeping world velocity continuous.

