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
> (the graph editor) and *✎ Animating* (the timeline).

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
preview, and the ✎ Animating timeline (keys + events become editable) — still
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

## 3. The ✎ Animating tab (timeline)

Select a node that has a controller (or a rigged model) and open **Window →
✎ Animating**:

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

Both are plain RON — hand-editable, diff-able, and discovered anywhere under
`assets/` by extension.

**Not yet built:** GPU vertex skinning for weighted/deforming meshes (the import
already captures joints/weights/inverse-binds; rigid per-node rigs like R6-style
characters are fully supported today).
