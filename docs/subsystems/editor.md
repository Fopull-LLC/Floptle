# Floptle — The Editor (`floptle-editor`)

> The app that ties everything together: dockable panels, a live wgpu viewport,
> and a Scene View where you *build geometry by drawing it*. See
> [`../decisions/0004-editor-egui.md`](../decisions/0004-editor-egui.md),
> the VSCode workflow [`../decisions/0011-vscode-integration.md`](../decisions/0011-vscode-integration.md),
> and the panels it hosts: [`./particles-vfx.md`](./particles-vfx.md),
> [`./shaders.md`](./shaders.md), [`./materials-and-textures.md`](./materials-and-textures.md),
> [`./asset-pipeline.md`](./asset-pipeline.md), [`./physics.md`](./physics.md),
> plus [`../ARCHITECTURE.md`](../ARCHITECTURE.md) §2.

`floptle-editor` is one of two binaries ([ARCHITECTURE](../ARCHITECTURE.md) §1).
Built on **egui + egui_dock** ([ADR-0004](../decisions/0004-editor-egui.md)) — pure
Rust, immediate-mode, rendered through the same wgpu device as the game, so editor
chrome and the live viewport coexist. It dogfoods the engine: building the editor
pressure-tests it.

## 1. Shell & theme

A docking shell (`egui_dock`): every panel is a tab you can split, stack, float,
or hide. Layouts are **customizable and persisted** per project; ship a few
presets (Scene, Shading, VFX).

```
┌──────────────────────────────────────────────────────────────┐
│ ☰ File  Edit  Scene  Build   ▶ Play  ⏹ Stop      [layout ▼]   │
├───────────┬──────────────────────────────────┬───────────────┤
│ Hierarchy │        Scene View (wgpu)          │  Inspector    │
│  ▸ World  │   ┌──────────────────────────┐    │  Transform    │
│   ▾ Floor │   │     live viewport         │    │  MeshRenderer │
│    • Cube │   │   gizmos · grid · pick    │    │  Collider ☑   │
│   • Light │   └──────────────────────────┘    │  Material ▸   │
├───────────┴──────────────────────────────────┴───────────────┤
│ Asset Browser │ Console / Profiler │ Particle Timeline ▸      │
└──────────────────────────────────────────────────────────────┘
```

**Theme:** dark, somewhat **high-contrast**, **retro / pixel-art-inspired** but
organized, readable, and clear (VISION §6, [ADR-0004](../decisions/0004-editor-egui.md)).
Crisp 1px borders, a tight pixel font option, saturated accent colors on a deep
neutral base. **Highly customizable**: a theme editor exposes palette, accents,
spacing, font, and corner radius as a `theme.ron` users can tweak and share.

## 2. Panels

Each is an `egui_dock` tab over the shared `EditorState`:

- **Scene View** — the live wgpu viewport; place, position, and **build geometry**
  (§3). The centerpiece.
- **Hierarchy** — the node tree (the Node facade over the ECS, [ADR-0005](../decisions/0005-scene-model-ecs-node-hybrid.md));
  reparent by drag, multi-select, rename.
- **Inspector** — a **modular component stack** (Unity-style). The selection shows
  *only the components it actually has* — its **Type** (geometry / camera / light /
  …, mutually exclusive), **Transform**, and any **Material / Rigidbody / Collider /
  Scripts** — each with a remove (🗑) and a **copy ⎘ / paste 📋** of its current
  values onto another component of the same kind. A **➕ Add Component** button at the
  bottom opens a **searchable, icon'd menu** to add the rest (or switch the Type). Make
  an **Empty** node and build it up from nothing; physics edits apply **live in Play**.
- **Asset Browser** — project assets; import-on-drop, drag-to-use, reimport
  ([`./asset-pipeline.md`](./asset-pipeline.md) §6).
- **Particle Timeline** — the video-editor-style VFX authoring surface
  ([`./particles-vfx.md`](./particles-vfx.md)).
- **Shader Graph** — node-graph view of the shader IR with an **Open in VSCode**
  button ([`./shaders.md`](./shaders.md) §6).
- **Material Editor** — assign shader, tweak params, drop textures, set tiling,
  live preview ([`./materials-and-textures.md`](./materials-and-textures.md) §6).
- **Console / Profiler** — log output + the lightweight in-engine **frame
  profiler** (per-pass GPU timestamps, the raymarch step heatmap from
  [`./renderer.md`](./renderer.md) §6). "Lightweight" is measured, not assumed.

```rust
struct EditorState {
    project:   Project,                // open project (paths, settings)
    scene:     SceneHandle,            // active scene (nodes ⇄ ECS)
    selection: Vec<NodeId>,
    gizmo:     GizmoMode,              // Translate | Rotate | Scale
    tool:      SceneTool,              // Select | DrawShape(ShapeKind)
    snap:      SnapSettings,
    layout:    DockLayout,             // egui_dock tree, persisted
    play:      PlayState,              // Editing | Playing | Paused
}
```

## 3. The Scene View — build geometry in-scene

The developer's exact vision: **interact, place, position, AND build geometry**
right in the scene — no round-trip to Blender for blockouts.

### Create menu

Right-click in the viewport:

```
Create new ▸
  ├─ Node                 (empty node — add components in the Inspector)
  └─ Shape ▸
       ├─ Cube
       ├─ Sphere
       ├─ Cylinder
       ├─ Capsule
       ├─ Wedge
       └─ Stairs   (property: number of steps)
```

### The creation gesture — draw the base, pull the height

Shapes are made by **drawing**, not dialog-filling:

```
 1) pick a Shape          2) DRAW the base on the ground   3) EXPAND UP for height
    (e.g. Cube)              (drag a footprint rectangle)     (drag the mouse up)
                            ┌───────────┐                    ┌───────────┐
       cursor ✦            │  footprint │                    │  █████████ │  ← height
                            └───────────┘                    │  █████████ │
        ground plane ───────────────────────────────────────┴───────────┴──
```

The footprint + height feed the shape's **parametric generator** — pure math from
the chosen `ShapeKind` and the drawn bounds produces the mesh. A Sphere's drawn
rectangle sets its radius bounds; Stairs lays `steps` treads across the footprint
rising to the pulled height; a Wedge slopes from one drawn edge.

```rust
enum ShapeKind {
    Cube,
    Sphere,
    Cylinder,
    Capsule,
    Wedge,
    Stairs { steps: u32 },
}

struct ShapeDef {
    kind:    ShapeKind,
    bounds:  Aabb,            // footprint (x,z) + pulled height (y)
    // regenerated whenever kind/bounds/params change
}

struct ShapeComponent {       // lives on the node; mesh + SDF derive from it
    def:        ShapeDef,
    collidable: bool,         // → SDF collider (floptle-physics)
    material:   AssetRef,
}
```

### Editable after creation

A `ShapeComponent` keeps its `ShapeDef` — it is **parametric forever**. Select the
shape and the Inspector shows its params (dimensions, `steps`, etc.); change one
and the mesh **regenerates** live. No baking into dead triangles. (Drag the
generated mesh into Blender only if you want to hand-sculpt beyond parametrics.)

### Easy per-shape setup

Right in the Inspector / on drop, set the things the developer wants to be trivial:

- **Collidable or not** — a checkbox. On → the shape's SDF is registered in the
  collision world ([`./physics.md`](./physics.md)); these primitives **double as
  SDF colliders** (analytic Cube/Sphere/Capsule/Wedge/Stairs distance functions),
  sharing the exact path the fractals use ([ARCHITECTURE](../ARCHITECTURE.md) §9b).
- **Material** — assign/drop a material ([`./materials-and-textures.md`](./materials-and-textures.md)).
- **Texture + tiling** — drag a texture on; auto-tiling (Repeat for good UVs,
  Triplanar for these procedural shapes) so tiling needs **no shader edit**
  ([`./materials-and-textures.md`](./materials-and-textures.md) §3, §6).

## 4. Gizmos, selection, snapping

- **Gizmos** — move/rotate/scale handles on the selection; `W/E/R` switch modes;
  drag a handle to transform, hold to constrain to an axis/plane.
- **Selection / picking** — click to pick (GPU id-buffer or ray-vs-AABB/SDF),
  box-select, `Hierarchy` and viewport selection stay in sync.
- **Snapping** — grid snap for translation, angle snap for rotation, and a vertex/
  surface snap so drawn footprints land cleanly. `SnapSettings` is configurable;
  hold a modifier to toggle snap on the fly.

## 5. Open in VSCode

Scripts (`.lua`) and textual shaders (`.flsl`) open externally
([ADR-0011](../decisions/0011-vscode-integration.md)): the editor shells out to

```
code <projectRoot> --goto <file>:<line>
```

so VSCode opens (or reuses) the **project as the workspace root** and focuses the
file/line. Triggered from the Inspector's script field, the Asset Browser
right-click, and the Shader Graph's **Open in VSCode** button. The "external editor
command" is configurable for non-VSCode users. No embedded code editor — that's
scope creep against "lightweight."

## 6. Project & play management

```rust
enum PlayState { Editing, Playing, Paused }
```

- **Projects** — open/create a project ([`./asset-pipeline.md`](./asset-pipeline.md) §4);
  `project.ron` holds settings + the entry scene.
- **Scenes** — create, save (RON), switch, and define **transitions** between
  scenes; an in-editor scene list.
- **Play / Stop** — **▶ Play** runs the game in-editor using the same
  `floptle-runtime` logic (frame loop, scripts, physics, vfx); **⏹ Stop** restores
  the edit-time scene. **Pause** + step for debugging. The Scene View becomes the
  game viewport while playing.

## 7. Out of scope

We do **parametric primitives, not a full DCC modeling toolset.**

- **Arbitrary mesh modeling** — no edge-loop/extrude/sculpt/retopo tooling. Build
  blockouts from parametric shapes here; do real modeling in **Blender** and
  import via glTF ([`./asset-pipeline.md`](./asset-pipeline.md)).
- **UV unwrapping / texture painting** — Blender's job; we lean on triplanar so
  procedural shapes tile without UVs anyway.
- **A built-in code editor** — VSCode via ADR-0011.
- **Animation rigging / weight painting** — authored in Blender; we import skins
  and play clips ([`./animation.md`](./animation.md)).

If a tool duplicates what Blender or VSCode already do well, it doesn't belong in
the editor.
