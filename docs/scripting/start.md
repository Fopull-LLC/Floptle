# Start here

Your first script, the hooks the engine calls, and the globals every script has.

Part of the [scripting guide](../scripting.md) · [every call, as a reference](../lua-api.md)

## Contents

- [1. A first script](#1-a-first-script)
- [2. Lifecycle: `start`, `update`, `fixedUpdate`](#2-lifecycle-start-update-fixedupdate)
- [6. Globals: `params`, `time`, `dt`, `log`](#6-globals-params-time-dt-log)
- [12. Recipe: a walkable first-person character](#12-recipe-a-walkable-first-person-character)
- [13. Bundled example scripts](#13-bundled-example-scripts)

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

---

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

---

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

---

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

---

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
