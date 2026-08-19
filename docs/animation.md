# Animation in Floptle

Floptle's animation system is built around three ideas:

1. **Baked animation clips** (`.anim.ron`) — standalone keyframe files, extracted
   from a model's glTF animations or authored in the editor. Channels bind by
   **node name**, so one clip plays on any rig with matching names — and on plain
   scene nodes (cutscenes, doors, platforms).
2. **Animation Controllers** (`.actl.ron`) — a visual graph of states with
   crossfade rules and **priority layers**. Code *triggers* states
   (`anim:play("Run")`); the controller supplies the blending.
3. **Stepped playback** — the retro low-framerate look (8/12 fps "choppy in a
   good way") is built in, per controller or per state, without ever desyncing
   transitions or events.

> Everything below is reachable from the **Window** menu: *Animation Controller*
> (the graph editor) and *⏱ Animating* (the timeline).

---

## 1. From a model to playable animations

1. Drop a `.glb`/`.gltf` with animations into your project (e.g. `models/`).
2. Select it in **Assets** — the Inspector lists its packaged animations
   (`▶ Animations (6)`).
3. Click **⬇ Extract animations**. Each clip becomes its own
   `animations/<Model>/<Clip>.anim.ron` file — organize them however you like
   afterwards (controllers find a moved clip by file name as long as it stays
   unique).

A rigged model **plays without any setup**: drop it into the scene, press Play,
and its `Idle` (or first) clip loops. Scripts can drive the embedded clips via
`node:animator()` immediately. Once a clip is **extracted**, the `.anim.ron`
with the same name takes over from the embedded copy everywhere — playback,
preview, and the ⏱ Animating timeline (keys + events become editable) — still
with no controller required. For real control, add a controller:

## 2. Animation Controllers

**Inspector → ➕ Add Component → Animation → Animation Controller (new)**, or
create one in the Assets browser (right-click → New Animation Controller).
Double-click a `.actl.ron` (or click **◉ Edit graph** on the component) to open
the graph editor:

- **Drag clips** (▷ `.anim.ron`) from Assets onto the canvas — each becomes a
  **state node**. The first becomes the default (▶) state.
- **Drag the ○ port** on a state's edge onto another state to add a
  **transition arrow**. Click an arrow to edit its fade time.
- **Fades**: the controller has a `default fade`; per-arrow overrides beat it;
  a state's **⇥ override incoming fades** beats everything — EVERY transition
  into that state uses its one fade time. Set it to **0** for a guaranteed
  instant snap, even with stepped playback on (it lands exactly on frame 0).
- **Per-state settings** (click a node): clip, speed, looped, ⇥ fade-in
  override, and an optional per-state stepped fps.
- **Stepped playback**: check **stepped** in the header (e.g. 12 fps) for the
  whole controller; a state's own fps overrides it. Time itself keeps flowing
  smoothly — only the *sampling* snaps — so transitions and events never drift.

### Layers

Layers are a priority stack: **left = base, right = higher priority**. A playing
state on a higher layer **overrides the nodes its clip animates**; everything
else shows through from below, and the **weight** slider blends the whole layer.

The classic setup: a **Movement** base layer (Idle/Walk/Run/Jump with
transitions), plus an **Attack** layer above it containing one-shot attacks. When
a script calls `anim:play("Slash")`, the attack takes over; when the one-shot
finishes, the layer releases automatically and movement shows again. If the
attack clip only animates the arms, the legs keep walking.

## 3. The ⏱ Animating tab (timeline)

Select a node that has a controller (or a rigged model) and open **Window →
⏱ Animating**:

- Pick the **animation** from the dropdown (the controller's states).
- **Scrub** the ruler to preview; **⏵** plays a live preview; **⏹** restores the
  scene pose. Previews never dirty the scene — undo/save always see authored
  transforms.
- **Events lane (⚑)**: *Add event at playhead* → the flag calls a Lua function
  (by name) on the node's scripts when the playhead crosses it during Play.
  Drag flags to retime; right-click to delete. Great for footsteps, hit frames,
  spawning VFX.
- **Key rows**: one row per animated node; diamonds are keys (the union of the
  T/R/S lanes). Drag to retime, right-click to delete. `snap` quantizes to a
  frame grid (8/12/24/30/60 fps).
- **● Record** (scene animation): with record on, pose the node's children with
  the gizmo or Inspector and **keys are written at the playhead** for whatever
  you moved. Scrubbing previews what you've keyed so far, so scrub → pose →
  scrub → pose is the whole authoring loop. Recording edits the **clip**, never
  the scene: turning record off (or ⏹) restores the exact pose the subtree had
  when you turned it on. Use **✚ New…** to start a fresh empty clip (it's added
  to the controller too).

### Property lanes — what can be keyed

**✚ Property ▸ node ▸ component ▸ field.** Components are grouped, and each
component puts the handful of fields people reach for at its top level with the
rest one step in (*Colour*, *Surface*, *Sheet*, *Layout*, *Flags*), so a
material's three dozen animatable fields do not bury its texture and its opacity.

| Component | What you get |
|---|---|
| **Material** | `texture`, `opacity`, `ambient`; **Colour** — base `r/g/b`, emissive, specular, shininess, rim; **Sheet** — `cell`, `sheetCols`, `sheetRows`; **Surface** — the maps and their strengths, roughness, metallic, reflectivity, transmission, ior, thickness; **Flags** — `unlit`, `fog`, `shading`, `jitter` |
| **UiElement** | `image`, `opacity`, `visible`, `text`; **Layout**, **Colour**, **Text**, **Sheet** |
| **PointLight** | `intensity`, `range`, colour |
| **UiSlider** | `value`, `min`, `max` |
| **Camera** | `fovY`, `orthoHeight` |
| **Sprite** | `frame` — see below; **Node** — `ppu`, `size`, `pivotX`, `pivotY`, `flipX`, `flipY` |

The names in the table are the groups; the **field names** are what a `.anim.ron`
stores, and are what you type if you hand-edit one. Colours are per channel:
`r` `g` `b` for a material's base colour, `emissiveR/G/B` + `emissiveStrength`,
`specularR/G/B` + `specularStrength` + `shininess`, `rimR/G/B` + `rimStrength`.
A UI element's are `fillR/G/B/A`, `textR/G/B/A`, `tintR/G/B/A`. `opacity` is a
material's alpha, and `orthoHeight` is a 2D camera's zoom.

`visible`, `unlit` and `fog` are numbers because a keyframe holds one: **0 is
off and 1 is on**, and they are created as stepped lanes so nothing is ever half
on.

**● Record picks all of them up**, not just the numbers: change a material's
texture with record on and a stepped `Material.texture` lane appears with a key
at the playhead, exactly as changing its opacity gives you a smooth
`Material.opacity` lane. Fields whose numbers are indices or flags rather than
quantities — a spritesheet `cell`, `unlit` — are created **stepped**, because
half a cell is not a cell.

Recording authors the clip and never the scene: turning record off puts every
value back, paths included.

> A field is only animatable if the engine can both **read** and **write** it —
> read so record notices it change, write so a key plays back. A test walks the
> whole list and asserts both, because each half fails silently on its own.

### Sprite lanes

**Sprite** is one heading with two halves. `frame` is the picture; **Node** is
what a ▫ Sprite node does with it.

**✚ Property ▸ Sprite.frame** adds a sprite lane, on any node wearing a
material. Each key holds a **whole frame** — the image, how it is cut, and which
cell — picked together, because they are drawn together: a texture and a grid
that land on different keys give you a cell index read against the wrong grid,
which draws a slice of the wrong picture and reports nothing.

A sprite lane is a **step** lane and cannot be anything else — the conversion
forces it whatever the file says. Interpolating two frame references is
meaningless, and interpolating two cell *indices* plays every cell in between,
which reads as the clip running at the wrong speed rather than as a bug.

**✚ Property ▸ Sprite ▸ Node** keys the ▫ Sprite node's own numbers: `ppu` and
`size` (squash and stretch), `pivotX`/`pivotY` (shift the origin for a crouch),
and `flipX`/`flipY` (face the other way on a turn). These do nothing on a node
that is not a ▫ Sprite, and `frame` — which writes the *material* — works on
anything wearing one.

The flips are **stepped**, like every other on/off lane: **0 is left alone and 1
is mirrored**. An eased flip would turn the sprite round exactly halfway between
two keys, at a moment nobody authored.

The other way to author sprite animation is a
[`.spriteanim.ron`](2d.md#sprite-animation--a-frame-names-its-own-art) — a frame
list with a frame rate, which is how a walk cycle is actually written down. It
loads as an ordinary clip, so both ways end up in the same controller, the same
crossfades and the same script API. Use a lane when the sprite is *part* of what
is animated (a character whose picture changes and whose node moves is one clip,
not two); use the file when the clip is frames and nothing else.

The Animating tab plays a `.spriteanim.ron` but does not edit it: the timeline
writes keyed lanes, which is a different shape and a different filename, so
saving one as the other would leave two files claiming one clip.

### Two states, one clip

A **state** in a controller is a name plus a pointer to a clip *file*. Nothing
stops two states pointing at the same file — that is how you reuse one `Hit`
clip across three attacks, and it is a real authoring choice. But it also means
those states are **one animation**: key it under either name and both change.

So the ⏱ Animating tab says so. A shared clip is marked **⚠ shared** in the
animation dropdown, and selecting one shows a banner naming every state that
plays it, with two fixes:

- **Give this state its own copy** — copies the clip to a new file named after
  the state and repoints only this state at it. The others keep sharing.
- **Split every shared state** — one click for a whole controller: the first
  state to use a clip keeps it, everyone else gets their own copy. This is the
  one to reach for after generating a controller in bulk (a stack of states
  wired up by hand or by a script often share a handful of placeholder clips),
  before you start animating.

Neither is automatic: sharing that you *meant* stays shared.

Model-embedded clips that haven't been extracted are previewable but not
editable — click ⬇ Extract on the model and the timeline opens the extracted
files instead (no controller needed). Bone-level re-authoring stays in
Blender; events + timing live here.

Scene clips bind channels by **node name relative to the controller's node**
(`""` = the node itself), so a `DoorOpen` clip written against one doorframe
retargets to any other node tree with the same child names.

## 4. Scripting

```lua
-- movement.lua — drive a character's controller from physics state.
local anim
function start(node)
  anim = node:animator()
end

function update(node, dt)
  local speed = math.sqrt(node.vx^2 + node.vz^2)
  if not node.grounded then
    anim:play("Jump")                 -- controller fade table applies
  elseif speed > 6 then
    anim:play("Run")
  elseif speed > 0.5 then
    anim:play("Walk")
  else
    anim:play("Idle")
  end
  if input.pressed("j") then
    anim:restart("Slash")             -- one-shot on the Attack layer
  end
end

-- called by a ⚑ event placed on the Slash clip:
function onSlashHit(node)
  log("hit frame!")
end
```

- `anim:play(state [, fade [, layer]])` — safe to call every frame (re-playing
  the current state never restarts the blend).
- `anim:restart(...)` — force re-entry (re-trigger a one-shot).
- `anim:crossfade(state, fade [, layer])` — explicit fade.
- `anim:stop([layer [, fade]])`, `anim:setSpeed(x)`,
  `anim:setLayerWeight(layer, w)`, `anim:seek(t [, layer])`.
- Reads: `anim:state([layer])`, `anim:time([layer])`, `anim:finished([layer])`,
  `anim:isPlaying([state])`, `anim:clips()`, `anim:layers()`.
- Authored data, from the asset rather than playback (so it works in `start()`):
  `anim:duration(clip)` and `anim:events(clip)` → `{ {t, func}, ... }` ascending
  by `t`. A game with integer frame data bakes these once at load instead of
  letting events drive gameplay — see the scripting reference.

Ordering each frame is **scripts → animation → physics**: what you set this
frame shows this frame.

> **Animation vs physics:** because physics integrates last, a node with a
> dynamic **RigidBody** is owned by the simulation — scene-animating its
> transform has no visible effect. Animate plain nodes (doors, platforms,
> cameras, props) and give them **Collidable** if things should bump into them;
> drive rigidbodies from scripts via velocities instead.

## 5. The retro stepped look

Set **stepped** on a controller (say 12 fps) and every state samples on that
frame grid — the classic hand-animated choppiness. Override per state (a snappy
8 fps attack over 24 fps movement is a great combo). Two guarantees:

- **Transitions stay exact**: a state whose fade-in override is 0 shows its
  frame 0 the moment it's triggered — the frame grid never delays it.
- **Events stay exact**: event timing uses real (smooth) time, so a footstep at
  `t = 0.43s` fires at 0.43s regardless of the visual frame rate.

## 6. File reference

| Asset | Extension | Home (default) | Notes |
|---|---|---|---|
| Animation clip | `.anim.ron` | `animations/<Model>/` | self-contained keys + events, name-bound |
| Animation controller | `.actl.ron` | `animation_controllers/` | layers, states, fade table, stepped fps |
| Sprite animation | `.spriteanim.ron` | beside the art | a frame list with an fps — loads **as a clip** |

All three are plain RON — hand-editable, diff-able, and discovered anywhere under
`assets/` by extension.

## Skinning happens on the GPU

A weighted mesh is deformed in the vertex shader. The bind pose is uploaded once,
at import, along with each vertex's four joint slots and weights; every frame each
character supplies only its **bone palette** — one matrix per joint — and draws
the same shared buffer.

What this changes for a game is the ceiling on how many animated characters can be
on screen. The old path deformed every vertex of every character on the CPU and
re-uploaded the result to a vertex buffer that had to be **private per character**,
because two characters sharing one `.glb` would otherwise share one buffer and the
last one posed would win for both. A mid-detail character — 8,000 vertices over 24
joints — cost 0.114 ms of CPU per frame, so fifty of them spent a third of a 60 fps
frame before anything drew. The same fifty now cost the CPU 24 matrix multiplies
each, and because they share one buffer again they are **one draw call**.

The CPU deform is still there, and still tested, as the fallback for a part the
skinning store cannot take (it is bounded at ~8.3M skinned vertices per scene by
the instance lane that addresses it) and for a part drawn by a custom `.flsl`
material, whose pipeline has no skinned variant. The two paths are held to
producing the same picture by `cargo run -p floptle-render --example skin_probe`,
which renders one posed mesh both ways and fails if they disagree.
