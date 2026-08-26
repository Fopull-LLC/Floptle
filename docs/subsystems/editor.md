# Floptle — The Editor (`floptle-editor`)

> The app that ties everything together: dockable panels, a live wgpu viewport,
> and a Scene View where you *build geometry by drawing it*. See
> ADR-0004,
> the VSCode workflow ADR-0011,
> and the panels it hosts: [`./particles-vfx.md`](./particles-vfx.md),
> [`./shaders.md`](./shaders.md), [`./materials-and-textures.md`](./materials-and-textures.md),
> [`./asset-pipeline.md`](./asset-pipeline.md), [`./physics.md`](./physics.md),
> plus [`../ARCHITECTURE.md`](../ARCHITECTURE.md) §2.

`floptle-editor` is one of two binaries ([ARCHITECTURE](../ARCHITECTURE.md) §1).
Built on **egui + egui_dock** (ADR-0004) — pure
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
organized, readable, and clear (VISION §6, ADR-0004).
Crisp 1px borders, a tight pixel font option, saturated accent colors on a deep
neutral base. **Highly customizable**: a theme editor exposes palette, accents,
spacing, font, and corner radius as a `theme.ron` users can tweak and share.

## 2. Panels

Each is an `egui_dock` tab over the shared `EditorState`:

- **Scene View** — the live wgpu viewport; place, position, and **build geometry**
  (§3). The centerpiece.
- **Game View** — the active camera's shot, exactly as a build shows it. It draws
  through one of two paths depending on where it is: fullscreen (double-click the
  tab) it renders straight to the window surface, docked it renders into an
  offscreen target sized to its own panel and is blitted there.

  **Those two must not look different**, and keeping them the same is a standing
  hazard rather than a solved problem — the two gathers have drifted five times.
  Four of those were geometry a docked panel never drew; the fifth was subtler,
  and worth knowing about because it is the shape the next one will take: the
  offscreen path ran no opaque depth prepass, so contact shadows, `surfaceGap`
  (shoreline foam, soft particles), screen-space reflections and lamp shadows all
  read an empty depth texture, each took its "nothing to report" branch, and drew
  nothing. Nothing was missing from the picture — four features were simply
  absent from it. `tests/offscreen_draws_the_same_world.rs` now covers passes as
  well as gathers.

  The panel takes its whole tab body, margin included. `egui_dock` insets every
  body by `spacing.window_margin`, which is right for a panel of widgets and
  wrong for a view: the Game tab is transparent so the 3D can show through, so
  the inset left a band of the EDITOR's render of the scene visible all the way
  round the game — a border that moved when you orbited, because that is what it
  was. The viewport rect is expanded to the full body rather than merely painted
  over it, so the picture is rendered at the size it is shown at and the pointer
  still maps to where it looks like it does.
- **Hierarchy** — the node tree (the Node facade over the ECS, ADR-0005);
  reparent by drag, multi-select, rename.
- **Inspector** — a **modular component stack** (Unity-style). The selection shows
  *only the components it actually has* — its **Type** (geometry / camera / light /
  …, mutually exclusive), **Transform**, and any **Material / Rigidbody / Collider /
  Scripts**, each indented under its header with a **…** overflow menu to copy / paste /
  remove it (paste targets another component of the same kind). A **➕ Add Component**
  button opens a **searchable menu** (auto-focused for typing) to add the rest or switch
  the Type. Make an **Empty** node and build it up from nothing; physics edits apply
  **live in Play**.
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

### Multi-select editing (v0.49)

The Inspector shows the **last node picked** and edits it — and hands whatever it
changed to the rest of the selection. Only what changed travels: set roughness on
twelve crates and each keeps its own colour, texture and everything else.

It works by DIFFERENCE, because an immediate-mode panel leaves no other record of
what it touched: `multi_edit::Snapshot::take` clones the primary's components
before the panel draws and `apply` compares them afterwards, field by field. Every
struct's diff destructures **exhaustively** — no `..` — so adding a field to
`Material` or `RigidBody` fails to compile there until someone decides whether it
should travel. Three things deliberately do not: a `Terrain`/`MapMesh` id (two
nodes on one field is data loss, not an edit), a scene singleton like the Skybox
or PostProcess node, and a camera's `active` flag or render-target name (both are
identities). `Transform` diffs per axis, or typing a height would align nothing.

`Editor::selected_group(e)` is the other half: every ✚ / ✖ component button and
the script-drop path loop over it, so "add a rigid body" with twelve selected
means twelve rigid bodies.

### Undo walks through what you selected (v0.60)

Picking a node is an undo step of its own. Ctrl+Z steps back through *selections
as well as edits*, in the order they happened, and undoing an edit hands you back
the node you were editing rather than dropping the selection on the floor — which
is what made undo feel like it had gone one step too far.

Three rules make that safe:

- **A selection step is history, not an edit.** It does not mark the scene
  unsaved and it does not clear the redo stack. Undo a move, click another node
  to check something, press Ctrl+Y: the move comes back. A click must never make
  an edit unrecoverable.
- **A selection that moved *because* of an edit belongs to that edit.** Deleting
  a node clears the selection; that is one step, not two, so undoing the delete
  restores the node *and* re-selects it.
- **Refs, not entities.** A step stores node *indices* in `query::<Matter>()`
  order — the order the scene serializer writes — because `restore()` respawns
  the world and an `Entity` would dangle. `Editor::begin_history_frame` is the
  one place a frame's selection diff becomes a step; a change that resolves to
  the same refs (a scene load swapping the world) mints nothing.

### The save indicator is always on screen (v0.60)

At the right end of the menu bar, wherever you are docked: a quiet `✔ saved`, an
amber `● unsaved` the moment an edit lands, and a green glow when a save
completes. Clicking it saves. Right-aligned in its own layout so nothing else on
the bar ever moves, and it is a chip rather than a toast because "did that
save?" is a question people ask *later*, when the toast is long gone.

It reads one flag, and that flag is aggregated over **everything a save writes**
— the scene `.ron`, terrain fields, vertex and texture paint, map geometry, the
texture palette. A sidecar that failed to write keeps the scene dirty and raises
the failure toast, because a permanent widget claiming "saved" while an hour of
sculpting sits only in memory is worse than no widget at all.

### The rig in the viewport (v0.49)

A selected rigged mesh draws its skeleton in the Scene view — sticks between the
joints, a dot on each, the selected one ringed — and **clicking a joint selects
it**, so the transform gizmo poses it straight into the open clip. Only the
selected mesh's rig is drawn: every rig at once buries the picture in white
sticks, and the one being posed would be the hardest to find.

Picking is `viz::pick_joint` over the projected joints within `BONE_PICK_PX`,
nearest-to-camera winning a contested click, and it runs *before* the node pick —
the rig is only on screen for a mesh already selected and is drawn over the
model, so a click that lands on a joint meant the joint. Selecting a bone clears
the node selection (they are mutually exclusive, the same swap the Hierarchy
makes), and the overlay keys on the selection *plus* the bone's own mesh so the
rig survives that clear. Under **Rig bones** in the gizmo filter menu.

## 5. Open in VSCode

Scripts (`.lua`) and textual shaders (`.flsl`) open externally
(ADR-0011): the editor shells out to

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

## The depth prepass and the two render paths (v0.53.1)

Everything that reads the opaque depth prepass — contact shadows, `surfaceGap`
(shoreline foam, soft particles), screen-space reflections and lamp shadows —
does nothing at all without it, and does it silently. That makes the prepass the
single most drift-prone thing in the editor's two render paths, and it has now
drifted three times:

1. the bind lived inside the `if rm_draw` arm, so every one of those features
   worked in a scene with terrain and silently did nothing in a scene made of
   meshes;
2. the offscreen path ran no prepass at all, so a docked Game panel showed a
   different game from the same game fullscreen;
3. the window path *bound* it only when `depth_prepass_with` reported having
   ALLOCATED a target — which is permanently false once a frame draws two views,
   because the size-keyed cache then finds both slots already there. From that
   frame on the window drew with whatever the docked Game panel had bound last:
   another camera's depth buffer at another resolution, and that panel's stored
   picture. Reflections landed wrong or vanished, and any resize made them
   briefly correct again.

And the *condition* had drifted separately: the window's list was missing contact
shadows, so a mesh scene with reflections and lamp shadows both off ran no
prepass in the window while the Game panel ran one.

Two functions now, both shared:

- **`wants_prepass(...)`** — the one answer to "does this view need it". Adding a
  feature that reads the prepass means adding a parameter, which is a compile
  error at both call sites rather than a silent omission at one.
- **`prepass_and_bind(...)`** — runs it and binds it, in one call. Running is not
  binding; the two are not separable here, so they are not separable at the call
  site either.

`Raster::depth_prepass_with` returns **nothing** now. It used to answer "was the
target reallocated?", which reads like "does the bind need refreshing?" and is a
different question. `tests/offscreen_draws_the_same_world.rs` requires both
functions by name in the offscreen path.

## Nothing may be hidden that cannot be un-hidden (v0.54.0)

The Hierarchy folds every parent on the first draw after a scene loads, so an
opened scene reads as a list of top-level things rather than as everything at
once. Whether a row gets a disclosure triangle was a *different* question:
`is_folder && has_kids`, where `is_folder` meant `Matter::Empty`.

The two disagreed, and the disagreement was silent and permanent. A Reflection
Probe parented to a Plane, a light parented to a mesh — anything under a node
that is not an Empty — was folded shut by the first rule and had no triangle to
open it under the second. Its children left the panel for good: still in the
scene, still saved, still loaded, simply unreachable. Adding another child added
to the pile. It reads as "the children I added just vanished", which is not a
sentence anybody connects to the word *hierarchy*.

`row_expandable(has_kids, has_bones)` is the fix and the invariant: **a node with
children IS a folder, whatever else it also is.** `fold_all_parents` is now a free
function next to it, and the test that guards them asserts the property rather
than the behaviour — for every row the fold hides, `row_expandable` must answer
true. A row that can hide children must be able to reveal them.

The icon changed too. A non-folder with children used to be drawn with `⏷`, the
*expanded* triangle glyph, in the icon column — so a collapsed unreachable
subtree announced itself as already open, which is why the panel looked correct
while being wrong.

## 7. Out of scope

This section has narrowed over time (texture painting and the embedded IDE both
landed after it was written, and the **map-building suite** —
[`../map-tools.md`](../map-tools.md) — added real in-editor
blockout modeling via the ▦ Map tool: draw a shape by dragging out its base
then its height, vertex/edge/face editing with normal-aligned move/rotate/scale
gizmos, extrude/inset/bridge/subdivide, a knife, per-face materials, and vertex
+ texture painting that survives geometry edits). What still holds:

- **Character/prop modeling** — sculpt, retopo, precise UV unwrapping, subdiv
  surface work stay in **Blender**, imported via glTF
  ([`./asset-pipeline.md`](./asset-pipeline.md)). Map meshes are for LEVELS.
- **Animation rigging / weight painting** — authored in Blender; we import
  skins and play clips ([`./animation.md`](./animation.md)).

If Blender does it better AND it isn't core to building a level in-engine, it
doesn't belong in the editor.
