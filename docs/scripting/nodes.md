# Nodes & the scene

Moving things, finding things, and swapping the whole world out.

Part of the [scripting guide](../scripting.md) · [every call, as a reference](../lua-api.md)

## Contents

- [3. `node` — the transform](#3-node-the-transform)
- [8. Referencing other nodes & scripts](#8-referencing-other-nodes-scripts)
- [17. Scenes: `scene.load` & the entry scene](#17-scenes-sceneload-the-entry-scene)
- [18. Layers & tags](#18-layers-tags)
- [21. Prefabs: `spawn` & `destroy`](#21-prefabs-spawn-destroy)

---

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

---

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
| `mgr.kind` | which script this is (its file name) |
| `mgr.valid` | is the script still loaded? |

> **Three names belong to the handle, not to your script.** `node`, `kind` and
> `valid` are answered by the handle itself, so a script that exports one of
> them can use its own copy and **no other script can** — a cross-script
> `h.kind` reads the handle's value instead of yours. The trap is that it is
> silent: the handle resolves, the field is there, the type is wrong, and
> nothing raises until something *calls* it. So the editor lints the export and
> the Console says so when the script loads.
>
> `name` is **not** reserved. A script's own `name` wins — `materials.name(id)`
> returning a display name is the obvious thing to write, and it used to be the
> one thing you could not. Ask `kind` when you want to know
> which script a handle is.

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
[§5](input.md#5-input-keyboard-mouse):

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

## 21. Prefabs: `spawn` & `destroy`

A **prefab** is a reusable node (with its whole child subtree) saved as an
asset. Make one by **dragging a node from the Hierarchy into the Assets
panel** (drop on a folder to aim; it lands in `prefabs/` otherwise), or
right-click the node → **◇ Save as Prefab**. Place instances by dragging the
prefab into the viewport, dropping it on a Hierarchy row (spawns as that
node's child), or right-click → **Add to scene**.

**To change a prefab, open it on its own:** double-click it in the Assets panel
(or right-click → **◇ Edit on its own**). Its nodes become the whole viewport —
same Hierarchy, same Inspector, same gizmos, same undo, and you can press Play —
and **Save writes back to that prefab file, in place**. Open any scene to go
back to editing a scene.

Two things a prefab does not carry, because it is nodes and nothing else:
terrain and blockout map geometry, which live beside a *scene* file. A prefab
holding a Map Mesh node will show the node and not its geometry.

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
[body modes](physics.md#4-node-the-physics-body)), its scripts fire `start` next pass,
animators/particles/audio wire themselves. Everything is undo-free play-state
— Stop discards it like any other play change.

**Multiplayer**: `spawn()`/`destroy()` are LOCAL. For replicated objects, the
server calls `net.spawn("bullet", {x=…, y=…, z=…})` — it accepts prefab names,
and spawns the whole subtree, so a player rig or a creature goes over the wire
as one thing — and `net.despawn(node)`, which broadcast to every client. `destroy()` on the server also routes
replicated nodes through the session automatically; on a client it refuses
(server authority).

**Gotcha**: a spawned prop that should be *solid* needs a Rigidbody in
**Static** mode (a plain Collidable marker only bakes at Play start).
