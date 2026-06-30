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
7. [Recipe: a walkable first-person character](#7-recipe-a-walkable-first-person-character)
8. [Bundled example scripts](#8-bundled-example-scripts)
9. [Tips & gotchas](#9-tips--gotchas)

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
| `input.pressed("space")` | `true` only on the frame it goes down (an edge) |
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

## 7. Recipe: a walkable first-person character

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

## 8. Bundled example scripts

Every project ships these under `scripts/` — open one for a working start:

| Script | What it does |
|---|---|
| `character.lua` | First-person walkable character (free-look, run, crouch, jump; planet-aware) |
| `freelook.lua` | Free-fly camera (right-mouse look, WASD, Shift to boost) |
| `rotate.lua` | Spin a node about Y |
| `pulsate.lua` | Animate scale over time |
| `float.lua` | Bob up and down |

## 9. Tips & gotchas

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
