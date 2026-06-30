# Scripting in Floptle (Lua)

Game logic in Floptle is written in **Lua**. A script is a `.lua` file in your
project's `scripts/` folder. Attach it to a node and it runs every frame while the
game is playing. Scripts **hot-reload** — save the file and the running game picks
it up immediately.

> The same reference is built into the editor: open the **Scripting** tab → **§ Docs**,
> and the API shows up as autocomplete + hover hints as you type.

## Contents
1. [A first script](#1-a-first-script)
2. [Lifecycle: `start` and `update`](#2-lifecycle-start-and-update)
3. [`node` — the transform](#3-node--the-transform)
4. [`node` — the physics body](#4-node--the-physics-body)
5. [`input` — keyboard & mouse](#5-input--keyboard--mouse)
6. [Globals: `params`, `time`, `dt`, `log`](#6-globals-params-time-dt-log)
7. [Assets & swapping models / materials](#7-assets--swapping-models--materials)
8. [Referencing other nodes & scripts](#8-referencing-other-nodes--scripts)
9. [Recipe: a walkable first-person character](#9-recipe-a-walkable-first-person-character)
10. [Bundled example scripts](#10-bundled-example-scripts)
11. [Tips & gotchas](#11-tips--gotchas)

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

## 2. Lifecycle: `start` and `update`

```lua
function start(node)        -- optional; runs once when Play begins
end

function update(node, dt)   -- runs every frame while playing
end
```

Each attached script keeps its **own state across frames** — assign a variable in
`start` (or at the top level) and read it back in `update`.

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

Key names: `a`–`z`, `0`–`9`, `space`, `enter`, `escape`, `tab`, `backspace`,
`delete`, `shift`, `ctrl`, `alt`, and arrows `left` `right` `up` `down`.

### Raycasting

`raycast(ox,oy,oz, dx,dy,dz, max)` casts a ray against the world's colliders (the
terrain **and** any walkable mesh colliders) and returns a hit table or `nil`:

```lua
-- ground within 1.2 units below me?
local h = raycast(node.x, node.y, node.z, 0, -1, 0, 1.2)
if h then
  -- h.x, h.y, h.z   the hit point
  -- h.nx, h.ny, h.nz the surface normal there
  -- h.distance       how far the ray travelled
end
```

Use it for ground checks, line-of-sight, shooting, or dropping objects onto a surface.
(The built-in `node.grounded` already does a robust contact check for the character;
raycast is the general-purpose tool for everything else.)

## 6. Globals: `params`, `time`, `dt`, `log`

| Global | Meaning |
|---|---|
| `params` | This instance's tunables — a table **seeded from `defaults`**, so `params.speed` works out of the box. The Inspector overrides individual values per node. |
| `time` | Seconds since Play started |
| `dt` | Seconds since the last frame (also the 2nd arg to `update`) |
| `log("…")` | Print to the engine **Console** |

The full Lua standard library (`math`, `string`, `table`, …) is available.

> **`defaults` → `params`:** every key you put in `defaults` is readable as
> `params.<key>`. Declaring `defaults` is what makes a value tweakable per-node in the
> Inspector; if you don't override it there, `params.<key>` is just the default.

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

> These work through the **node handle** too, so a manager script can re-skin any node it
> reaches: `find("Player"):getchild("Body").model = assets.getFile("models/hurt.glb")`.

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

```lua
-- A door that opens when the player is near it.
function update(node, dt)
  local player = find("Player")
  if not player then return end
  local dx, dz = player.x - node.x, player.z - node.z
  if dx*dx + dz*dz < 9 then node.y = 3 else node.y = 0 end   -- raise / lower
end
```

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

## 9. Recipe: a walkable first-person character

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

## 10. Bundled example scripts

Every project ships these under `scripts/` — open one for a working start:

| Script | What it does |
|---|---|
| `character.lua` | First-person walkable character (free-look, run, crouch, jump; planet-aware) |
| `freelook.lua` | Free-fly camera (right-mouse look, WASD, Shift to boost) |
| `rotate.lua` | Spin a node about Y |
| `pulsate.lua` | Animate scale over time |
| `float.lua` | Bob up and down |

## 11. Tips & gotchas

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
