# Assets & materials

Models, textures, materials and data files, swapped while the game runs.

Part of the [scripting guide](../scripting.md) · [every call, as a reference](../lua-api.md)

## Contents

- [7. Assets & swapping models / materials](#7-assets-swapping-models-materials)

---

## 7. Assets & swapping models / materials

Scripts can reach into the project's **`Assets/`** folder and change a node's
components at runtime — swap a mesh's model, apply a material — so one script can drive
a whole wardrobe of looks.

### `assets` — referencing files in code

`assets` resolves files by a path written **relative to `Assets/`** (the same path the
Asset Browser shows; right-click any asset ▸ **Copy asset path** to grab it). A path
that would leave the project — absolute, or through `..` — answers `nil` (or an
empty list) and one Console line naming the rule. `getContents` walks at most
20 000 files and says so if it had to stop.

| Call | Returns |
|---|---|
| `assets.getFile("models/armor.glb")` | the asset's path (a string you hand to `node.model` / `node.material`), or `nil` if it doesn't exist |
| `assets.getContents("models")` | an array of **every file** under that directory (recursive) — great for building tables |
| `assets.readText("data/intro.txt")` | the file's text, or `nil, why` |
| `assets.readJson("charts/neon.json")` | the file decoded as JSON (`json.decode` rules), or `nil, why` |
| `assets.writeText(path, text)` | write a text file under `Assets/` (folders created) — `ok, why` |
| `assets.writeJson(path, value [, {pretty=true}])` | encode and write a JSON file — `ok, why` |

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

**Your own data files.** A rhythm chart, a dialogue tree, a table of enemy
stats — anything you would rather author as data than as a Lua table — is a
JSON file under `Assets/`, and a script reads it in one call:

```lua
local chart, why = assets.readJson("charts/neon.json")
if not chart then return log("no chart: " .. why) end
for _, note in ipairs(chart.notes) do
  spawnNote(note.t, note.lane)
end
```

The reverse works too, so a chart editor can be a scene in your game rather
than a tool outside it — record the hits, then
`assets.writeJson("charts/neon.json", chart, { pretty = true })` writes a file
the Asset Browser shows, git diffs and the export ships. Decoding follows
`json.decode` (objects → tables, arrays → 1-based lists that stay lists on the
way back, `null` → `nil`); a file that is missing, not text or not JSON answers
`nil, why` plus one Console line, never an error that stops the script. Every
path is relative to `Assets/` and stays inside it. The same calls work in an
exported build and in a browser, where a write lands in the page's own storage.

**Pictures from the internet.** A player's profile picture, or an image another
player shared, becomes a texture while the game runs:

```lua
assets.textureFromUrl(avatarUrl, { headers = { ["X-Game-Key"] = key } }, function(tex, err)
  if not tex then return log("no picture: " .. err) end
  ui.make(row, { "image", w = 32, h = 32, texture = tex, radius = 16 })  -- round
end)
```

| Call | What it does |
|---|---|
| `assets.textureFromUrl(url [, opts], cb)` | download a picture and make it a texture; `cb(tex, err)`. `opts` as `http.get` |
| `assets.textureFromBytes(bytes, cb)` | the same, from bytes you already have (a blob's `res.body`) |
| `assets.release(tex)` | let a texture go; `true` if it was one of these |

`tex` is a name like `"img:3"` and goes anywhere a texture path does: `draw.quad`,
a UI image's `texture`, a material's `texture`. A UI image with `radius` half its
size draws round. The callback runs on a later frame, never inside the call.

The bytes are only ever read as pixels. PNG, JPEG and WebP are accepted, told
apart by the file's own signature rather than its name or the server's say-so;
anything else, or a picture wider or taller than 4096 pixels, answers
`nil, why`. A download is exactly an `http.get` (Play only, public addresses,
the same rate limits), and a URL already loaded this session answers from
memory without downloading again. Pictures are kept until `assets.release` or
Stop. A host that draws nothing, such as a dedicated server, answers every one
with an error and downloads nothing.

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

### `node:material(...)` — a model's materials, one at a time

A model arrives with its own materials — a character has a `Head`, a `Clothing`,
a `Pants` — and each of them draws on one or more of its parts. Two calls reach
them:

```lua
for _, slot in ipairs(node:materials()) do
  print(slot.material, slot.object, slot.textured, slot.overridden)
end
-- Clothing  Torso#2    true  false
-- Head      Head#2     true  false
-- Pants     RightLeg#2 true  false

node:material("Clothing").texture = assets.getFile("textures/shirt.png")
node:material("Pants").texture    = assets.getFile("textures/jeans.png")
```

**Ask before you address.** `node:materials()` is not decoration: import renames
repeated object names, so a model whose torso is called `Torso` in Blender is
`Torso#2` here, and a guessed name matches nothing. Every slot answers to two
names — the **object** (exactly one part) and the **material** (every part
wearing it, which is usually the group you mean: one `Clothing` covers a torso
and both arms).

A handle is the material, whole: `texture`, `normalMap`, `roughnessMap`,
`metallicMap`, `occlusionMap` by path (`""` clears one), `color` / `emissive` /
`specular` / `rim` as colours, and `alpha`, `roughness`, `metallic`,
`emissiveStrength`, `unlit`, `fog`, `cell` as values. Reads answer with what you
last wrote:

```lua
local shirt = node:material("Clothing")
if shirt.texture ~= wanted then shirt.texture = wanted end
```

`node:material()` with no name is the node's **own** Material — see the rule
below before reaching for it.

**A part's shader knobs go through the same handle.** A part that wears a
`.flsl` — skin on the head, a face decal, a cloth overlay on the torso — has
uniforms and texture slots of its own, and `node:setShaderParam` cannot reach
them: it writes the node's own Material, which on such a model usually does not
exist. The handle can:

```lua
local head = node:material("Head#2")
head:setShaderTexture("face", "faces/02.png")   -- the character creator's face swap
head:setShaderParam("blush", 0.6)
if head:shaderTexture("face") ~= "faces/02.png" then ... end   -- reads back, same frame
```

Two rules keep a typo from wrecking a part. A shader write lands only on an
override that **already exists and wears a shader** — it never creates one,
because an override is a whole material and creating one for a uniform would
blank the part to default white with nothing for the uniform to drive. And a
write with nowhere to land is said once in the Console rather than lost. Which
`.flsl` a part wears is authoring — set it in ◑ Model materials — and the
knobs are the runtime half. The node-level `node:setShaderParam` on a model
with overrides and no node Material fans out to every part that wears a shader,
which is the "everything glows" case.

### One material over a whole model

**A Material on a model supersedes the model's own materials.** Every part draws
with that one material — its colour, its texture, its maps. That is what the
component is *for*: "this whole thing is made of THIS". A model that already
looks right needs no Material at all.

So:

| What you want | What to use |
|---|---|
| this whole model is one material (all ice, all gold) | a **Material** on the node |
| this one part looks different | `node:material("<name>")`, or **◑ Model materials** in the Inspector |
| the model as the artist made it | no Material and no overrides |

An untextured material means untextured — a Material with no `texture` draws its
parts with no picture, rather than keeping the one it is replacing. Half-applying
a material is what made this confusing before: a new material would take effect
in its emissive and its numbers while the model kept the old picture on it.

### A tint — the same model, but red

A Material replaces. A **tint** multiplies over whatever a node already draws,
so the model keeps its textures and its parts keep their colours:

```lua
node:setTint(color(1, 0.3, 0.3))          -- hit flash
node:setTint(teamColor)                    -- team colours on one prefab
node:setTint(color(1, 1, 1), 0.4)          -- ghosted while being placed
node:setTint()                             -- back to normal
```

It needs no Material and does not create one. In the Inspector it is the **◐
tint** swatch; white at full opacity is "no tint" and removes it.

#### When a multiply is not enough

A multiply can only take light **away**, and that is the whole reason a team
colour so often ends up as a Material instead. Multiply a mid-toned, ambient-lit
character by a saturated "crimson" and what arrives is a slightly warm grey: the
eye reads lightness long before it reads hue, so the one thing the tint exists to
say — *which player is this* — is the thing it says worst.

So a tint also carries the two knobs that **add** light, in a table form:

```lua
node:setTint{ rim = teamColor, rimStrength = 1.3 }  -- an additive fresnel edge
node:setTint{ ambient = 1.6 }                        -- lift it out of the room's shadow
```

`rim` is an additive edge in its own colour. Because it adds rather than
multiplies, it reads on a dark costume and a bright one, against any stage, and
from across a room where the body fill is half in shadow — which is what actually
tells two fighters apart in motion. `ambient` multiplies this node's share of the
scene's ambient light, so a character can sit brighter than the room it is
standing in. That last one is what a Material carrying nothing but `ambient: 1.6`
was always being used for, and using a Material for it costs the model every
texture it was imported with.

**Fields you leave out keep their value.** The lanes are set at different times —
a character asks for its rim and its ambient once when it is dressed and rewrites
its *colour* on every hit flash — so `setTint(red)` after `setTint{ ambient = 1.6 }`
leaves the lift alone. Only `setTint()` with nothing takes the whole tint away.

A table is read as a colour unless it carries one of those names, so
`setTint{1, 0.5, 0.2}` is still a colour and every call that already exists keeps
working.

Because it is a component, an animation clip can key it — a flash is a tint
faded back to white over a fifth of a second — and a script can read it back:

```lua
local t = node:getcomponent("Tint")        -- nil when the node has none
if t then t.alpha = t.alpha - dt end       -- fade the whole model out
```

| What you want | What to use |
|---|---|
| this whole model is one material | a **Material** on the node |
| this model, but tinted | `node:setTint(color)` |
| this model, but it READS across the room | `node:setTint{ rim =, ambient = }` |
| this one part looks different | `node:material("<name>")` |

### Getting a model's own textures out

A `.glb` keeps its images inside itself, so nothing outside it can point at one.
Select the model (or its node) and press **⬇ Extract textures**: each material's
image is written beside the model as `<model>_textures/<material>.png`, and from
then on it is an ordinary project texture — paintable in the 🖼 Image tab,
assignable to any material, usable as the base layer a clothing system draws
over. Overriding one part of a textured model extracts that part's texture on
the spot and starts the override wearing it, so "override" never means "go
blank".

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
| `friction` | Grip, as a coefficient: a ramp holds this body while `tan(its angle) ≤ friction`. 0 is ice, 0.3 lets go at about 17°, 1 holds exactly 45°, and a grippier surface goes above 1. |
| `slopeLimit` | The steepest surface this body can stand on, in degrees (60 by default). Past it there is no ground under it and no grip holds it. |
| `restitution` | Bounciness 0..1 (0 = no bounce). |
| `gravity` | Gravity pull on this body (assign `true`/`false`; reads back 1/0). |
| `shape` | Body shape: 0 = sphere, 1 = capsule, 2 = box. |
| `radius` | Sphere/capsule radius. |
| `height` | Capsule total height. |
| `half_x` `half_y` `half_z` | Box half-extents. |
| `lock_x` `lock_y` `lock_z` | Freeze world-axis translation (e.g. lock Z for 2.5D). A lock engaging mid-play freezes the body **where it is right then**. |
| `lock_rot_x` `lock_rot_y` `lock_rot_z` | Freeze rotation about an axis (keep a body upright). Holds the rotation the node has when the lock engages. |

| `getcomponent("PointLight")` | Meaning (Inspector: ● Point Light / ◤ Spot Light) |
|---|---|
| `intensity` / `range` | Brightness multiplier / reach in world units. |
| `r` `g` `b` | Light color, 0..1 per channel. |
| `shadows` | Stop this lamp at the walls between it and what it lights. |
| `spotAngle` | **Aim it.** The FULL cone angle in degrees, down the node's local −Z. **180 or more is no cone** — an ordinary omnidirectional lamp, which is what every light reads until you change this. |
| `spotSoftness` | How much of the cone's edge is falloff, 0..1. A *fraction* of the cone, so widening a beam keeps the edge you gave it. |
| `shape` | The emitter: 0 point, 1 sphere, 2 rect, 3 disk, 4 tube. Switching keeps the size where two shapes share one. |
| `radius` `width` `height` `length` `thickness` | The emitter's dimensions. Each reads 0 on a shape that has no such dimension, and a write lands only on a shape that has it. |
| `twoSided` | A rect or disk that emits from both faces. |

A spot is a point light that has been aimed — same node, same range, same emitter
shapes, same slot in the sixteen. So there is no separate "is it a spot" flag to
keep in step: `spotAngle` **is** the answer, and 180 turns the cone off.

```lua
-- A searchlight sweeping a yard, tightening as it locks on.
local lamp
function start(node) lamp = node:getcomponent("PointLight") end

function update(node, dt)
  node:rotate(0, 30 * dt, 0)                     -- the node's forward IS the beam
  lamp.spotAngle = locked and 12 or 50
  lamp.spotSoftness = locked and 0.05 or 0.4     -- tight and hard, or wide and soft
end
```

> **Rotate the node to aim it.** The cone runs down local −Z, the same axis a
> camera looks down, so pointing a lamp and pointing a camera are the same
> gesture — and `node:lookAt(target)` aims a spot exactly as it aims a camera.

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
there:

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
draw.text(x, y, s, size, r, g, b [, a] [, align] [, font])
```

`draw.text` is measured and laid out by the engine with the same font stack
`ui.make` uses, so a damage number, a frame-time readout or a count under a
selection box needs no UI tree and no idea how wide an `m` is. `align` is
`"left"` (default) | `"center"` | `"right"`, and says which edge `x` is:

```lua
-- a HUD in three lines
draw.text(24, 24, "HP " .. hp, 22, 1, 0.4, 0.4)
draw.circle(40, 80, 12, 0.3, 1, 0.5, 0.8)
draw.text(w - 24, 24, string.format("%.1f fps", 1 / dt), 18, 1, 1, 1, 0.7, "right")
```

#### Which font it uses

Leave `font` out and you get **the project's UI font** — Project Settings ▸ **UI
font**, a project-relative `.ttf`/`.otf`. That is the setting to reach for: it
also covers every `ui.make` label and every element whose style names no font,
so a pixel-art game says its font once instead of at forty call sites.

```lua
-- one line, in the project's font
draw.text(24, 24, "HP " .. hp, 22, 1, 0.4, 0.4)
-- …and one that insists on a different one
draw.text(24, 60, "CHAPTER I", 30, 1, 1, 1, 1, "left", "fonts/Display.otf")
```

Set neither and you get the engine's built-in font, which is proportional. If
your layout assumes a **monospace grid** — a character at a time at a fixed
advance, which is how most typewriter dialogue is built — that mismatch does not
read as "wrong typeface". It reads as bad letter spacing: wide letters overlap
their neighbours and narrow ones leave holes, differently in every word.

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
| `changed(node)` / `submitted(node)` | a text field's value, or a draggable slider's, changed / Enter |
| `dragStart` / `dragMove` / `dropped` / `dragCancel` | on a `draggable` source |
| `dragEnter` / `dragOver` / `dragLeave` / `dropped` | on a `drop target` |

A gamepad **submit fires the same `clicked`** a mouse does, so a button written
for a pointer works with a pad and no second code path. See
[ui-navigation.md](../ui-navigation.md) for focus, text fields, drag & drop and
tooltips, and [ui-styles.md](../ui-styles.md) for what the states look like.

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

Full manual: **[ui-make.md](../ui-make.md)**. The short version:

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
value — `changed(node)` fires on each frame a drag moves it, and
`getcomponent("UiSlider").value` inside it is already the new value (a settings
volume slider is a draggable slider + a `changed` that saves it). Display-only meters
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
