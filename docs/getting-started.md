# Getting started: build a walkable scene

This is the fast path from an empty project to a level you can walk around in
first-person. It ties together the editor, terrain, physics, and scripting. For the
full reference on each piece see [scripting.md](./scripting.md) and
[physics.md](./physics.md).

> **Would rather build a whole small game?** The tutorials —
> [first steps](tutorials/first-steps.md), a [3D platformer](tutorials/platformer.md),
> a [top-down RPG](tutorials/topdown.md), [Flappy](tutorials/flappy.md) — walk
> you through a 3D platformer, a top-down RPG or Flappy from an empty project — and
> the editor's **🎓 Learn** tab has the same steps, ticking each one off as your
> project comes to match it. Start there if you are new to game engines; this page
> assumes you mostly want the tour.

## 1. The editor at a glance

- **Scene tab** — the editor view: tools, gizmos, click-to-select, sculpting.
- **Game tab** — the "as a build" view from the active camera. Editor input is
  suppressed here (clicks don't select, editor shortcuts don't fire); only your game's
  input runs. Press **F1** to Play / Stop, **F2** to pause.
- **Hierarchy / Inspector / Assets / Scripting** dock panels.
- Top-right of the viewport: a **◈ Gizmos** toggle hides every overlay for a clean view.

> **Stop reverts the world. Edits made during Play are not saved.**
>
> Play runs your game on a copy: moving a node, re-parenting one, or typing a
> number into the Inspector while playing all work, and all vanish the moment you
> Stop. They are not undoable and they never mark the scene unsaved, so Ctrl+S
> has nothing to write. A red **▶ PLAY** banner sits across the top of the
> viewport for the whole session as the reminder.
>
> That is on purpose — nudging a camera while watching a cutscene run is how you
> find the framing. To **keep** a value you found that way: open the **…** menu
> on the Inspector's Transform header, pick **Copy values**, Stop, then **Paste
> values** back onto the same node. The component clipboard survives Stop, so it
> lands as a real edit you can undo and save.

## 2. Sculpt some ground

1. **New ▸ Δ Terrain**. A flat slab appears.
2. Pick the **sculpt** tool (toolbar) and open the **Terrain** tab.
3. Raise / Lower / Flatten / Smooth with the brush. Sculpting near an edge grows the
   *bounds* (room to work) but no longer drags the ground outward, so you can make
   isolated hills, cliffs, and islands.
4. To lay deliberate flat ground, use **Fill bounds** (height / floor / inset).
5. **Paint** mode tints the surface or paints texture-palette slots; adjacent textures
   **crossfade** smoothly.
6. Toggle **View ▸ Terrain collider wireframe** to see exactly what you'll walk on.

> Terrain editing is fast: a single terrain uploads only the brush region per dab, and
> moving a terrain is instant. (Blending several terrains together is heavier — sculpt
> each one, then combine for final assembly.)

## 3. Add a player you control

The first-person recipe — no glue code:

1. **New ▸ ⌖ Camera**; in the Inspector mark it **active**.
2. Inspector ▸ **➕ Add Component → ◆ Rigidbody**, then set shape **Capsule**.
3. Drag **`scripts/first_person.lua`** onto it (drop it on the Inspector, or from Assets).

> The Inspector is a **modular component stack**: a node shows only the components it
> has, and **➕ Add Component** (a searchable menu at the bottom that focuses for typing
> the moment it opens) adds the rest — Rigidbody, Collider, Material, a script, or a
> different **Type**. Start from **New ▸ 🗀 Empty** and build a node up from nothing.
> Each component's **…** menu copies / pastes / removes it, so you can clone a
> component's values (a tuned Transform, a script's params) onto another node.

Press **F1** and switch to the **Game** tab. You *are* the capsule:

- hold **Right Mouse** to look · **WASD** to move · **Space** to jump
- **Shift** to run · hold **C** to crouch

## 4. Give the world gravity

A scene with no gravity node is **zero-g** — bodies float. To pull them down:

- **New ▸ ⬇ Gravity Volume**, mode **Down** → normal gravity everywhere.
- Mode **Radial** at a planet's center → a Mario-Galaxy planet: the character sticks and
  walks all the way around it.

Per-body, the Rigidbody inspector has **affected by gravity** — turn it off for a
floating object that still collides and can be script-driven.

## 5. Walk on an imported model

1. Drop a `.glb`/`.gltf` into `models/` and add it (it imports automatically).
2. Select the mesh node → Inspector ▸ **➕ Add Component → ▦ Collider**.
3. Play — the character collides with the model's triangles, not just the terrain.
   Toggle **View ▸ Mesh collider wireframes** to see them.

## 6. Script some interaction

Scripts are Lua files in `scripts/`, attached to nodes, hot-reloaded on save. A node's
script drives its transform — and, if the node has a Rigidbody, its velocity. Example: a
script that uses a raycast to snap a prop onto the ground under it:

```lua
function start(node)
  local h = raycast(node.x, node.y + 5, node.z, 0, -1, 0, 20)
  if h then node.y = h.y end
end
```

See [scripting.md](./scripting.md) for the full `node` / `input` / `raycast` API.

## Where to go next

- **[scripting.md](./scripting.md)** — the complete Lua API (transform, physics body,
  input, raycast, globals) and the character recipe in depth.
- **[physics.md](./physics.md)** — rigidbodies, gravity fields, colliders, raycasting,
  and how the play loop fits together.
- In-engine, the **Scripting ▸ § Docs** page mirrors the scripting reference with live
  autocomplete.
