# Maths & the world

Vectors, terrain, water, scattered props, and orbital time.

Part of the [scripting guide](../scripting.md) · [every call, as a reference](../lua-api.md)

## Contents

- [19. Vectors & math: `vec3`, `vec2`, `distance`](#19-vectors-math-vec3-vec2-distance)
- [22. Terrain: `terrain.sculpt`, `dig` & queries](#22-terrain-terrainsculpt-dig-queries)
- [22a. Water: volumes, buoyancy & `water.*`](#22a-water-volumes-buoyancy-water)
- [22b. Scatter: thousands of props from a seed](#22b-scatter-thousands-of-props-from-a-seed)
- [25. Space: orbits, gravity & time-warp](#25-space-orbits-gravity-time-warp)

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
| `v:withX(n)`, `v:withY(n)`, `v:withZ(n)` | the same vector with one component replaced — [prefer these to `v.x = n`](#two-vectors-and-which-one-your-project-uses) |
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

### Two vectors, and which one your project uses

Project Settings has a **Script vec3** choice (⚙ tab → **Scripting**), saved to `project.ron` as
`script_vec3`. It changes what a `vec3` is made of, and nothing else about
what you write.

| | `exact` | `fast` |
|---|---|---|
| components | 64-bit | 32-bit |
| can be changed in place | yes | **no** |
| allocates | one small object per vector | nothing at all |
| useful out to | the solar system | ~131 000 units from the origin |

**Every project that existed before this setting is `exact`**, written into its
`project.ron` the first time it is opened so it can never drift. New projects
start at `fast`. Nothing changes under a game that has already shipped.

`fast` is Luau's own vector type, which is why it costs nothing to make and
nothing to collect. How much that saves depends on how much of a frame's
allocation is vectors, and the answer is often less than it feels: on one
shipped game, switching moved per-frame allocation by 2%. `floptle run --alloc`
says what each script allocates — measure before switching for speed. The
price is precision: past about 131 000 units from the origin, 32-bit
components can no longer resolve a centimetre, and positions start to jitter.
If a project crosses that line the engine says so in the Console, once per
script, naming the setting — you will not have to work it out from the
symptom. A game whose world is bigger than that wants `exact`, and so does one
that keeps real distances in script (a solar system, a galaxy map).

**The one thing you write differently** is changing a component. A `fast`
vector cannot be assigned into:

```lua
-- Works in exact, RAISES in fast:
v.x = 0

-- Works in both, and is the better habit either way:
v = v:withX(0)
```

`withX` / `withY` / `withZ` exist in both modes, so you can write it this way
today and switch whenever you like. `node.x = …` and the other node fields are
untouched by any of this — a **node** is still mutable in both modes; it is the
vector *value* that is not.

To find out what a project would have to change:

```sh
floptle lint --vec3
```

It lists every component assignment and every `type(v)` check, with the line
and the fix, and exits non-zero if there is anything to do. It is a textual
scan rather than a type checker, so it cannot promise it found everything —
but a mutation it misses raises in `fast` rather than passing silently.

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

---

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

---

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

---

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
