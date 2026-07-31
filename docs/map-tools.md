# Building levels in the editor — the ⬢ Map tool

Blockout geometry you draw, cut, texture and **paint** without leaving Floptle.
Map meshes are for LEVELS — walls, floors, stairs, arches, ramps; characters and
props still come from Blender ([asset pipeline](subsystems/asset-pipeline.md)).

Press **8** (or Window ▸ ⬢ Map) to turn the tool on. The **⬢ Map** panel is the
full control surface; the strip over the viewport carries the controls you touch
constantly, so they're never behind another dock tab.

---

## 1. Draw a shape

Pick a shape — box, plane, wedge, cylinder, sphere, stairs, arch — then drag out
its **footprint** on the ground (or on any map surface you aim at), release, and
move to set its **height**. Click to commit.

While drawing: `,` / `.` turn it 90°, `Z` turns it around (for a staircase,
"climb the other way"), `[` / `]` change its resolution, Esc cancels.

New shapes arrive **collidable** and wearing the dev grid texture, because a
blockout you can measure by eye and walk on is the point.

## 2. Select: vertices, edges, faces

Three chips on the viewport strip — **◆ vertex**, **╱ edge**, **◼ face** — and
each has its own key (`J` / `K` / `M` by default; `Tab` cycles). Switching
**converts** your selection rather than dropping it: pick a face, press `J`, and
you're holding its four corners.

| Gesture | What it does |
|---|---|
| click | select what's under the cursor |
| **drag** | box-select — the box starts wherever you press, *including on the mesh* |
| Shift + click/drag | **add** to the selection |
| Ctrl + click/drag | **remove** from the selection |
| **All** / **None** / **Invert** | every one of the current kind, or none, or the complement |

Drag-anywhere matters more than it sounds: a blockout fills the screen, so a
box-select that could only start on empty space could rarely start at all.

Growing a selection: **Grow** (one more ring), **Connected** (the whole shell),
**Coplanar** (the flat region a face sits in), **Edge loop** (run an edge
selection through its quad loops), and **Select** on a material slot (every face
wearing it).

**Select through the surface** (off by default) decides whether sub-objects
hidden behind the mesh are clickable. Off means you stop grabbing the vertex on
the far side of a wall.

## 3. Shape it

- **⬆ Extrude** (`E`) pushes the selected faces out along their own normal —
  walls out of a floor. With grid snap on, it steps by the grid.
- **⊡ Inset** (`I`) shrinks a copy of each face inside its own border. Inset then
  extrude carves a recess (a window, a doorway).
- **⊞ Subdivide**, **⇌ Bridge** (join two faces with a tube of walls),
  **🗑 Delete**, **✂ Split off** (the selection becomes its own node),
  **⇄ Flip** / **Flip all**, **⊙ Weld**, **⌗ Snap to grid**.
- The gizmo does **move / rotate / scale** (`X` cycles) with handles aligned to
  the **selection's own normal**, its node, or the world (`V` cycles). Normal is
  the default: a diagonal wall pushes straight out of itself in one drag.

### ✂ Knife

`/` arms the knife. Click one **edge or corner** of a face, then another on the
same face, and the face splits along that line. The point under the cursor is
shown live — a filled dot for a new corner mid-edge, a ring for an existing one,
so you can see which you're about to get.

The cut carries into the faces that share those edges: they gain the same corner,
so the seam stays welded and no hairline crack opens along it. Both halves keep
the face's material slot.

After a cut the knife **keeps going** from the corner it just made, so a groove
can be walked across a face (or from one half into the next) in one gesture. Esc
ends the cut; Esc again puts the knife away.

A cut that can't divide a face — two points on one edge, two corners already
joined — is refused with the reason, and nothing is changed.

## 4. Materials, per face

The whole mesh takes an ordinary **Material** in the Inspector. To make *some*
faces different: select them and press **◑ New material for selected faces**.
That makes a **slot**, assigns your faces to it, and gives it its own material in
one step.

Each slot then has its own colour / texture / shader block, and a **Select**
button that selects every face wearing it. UVs are a box projection at 1 unit =
1 tile, so textures land at a consistent scale across a whole level with no
unwrapping — and **SIZE** resizes the *geometry* rather than the node, so a
resized wall keeps its texture scale instead of stretching it.

## 5. Painting a blockout

Map meshes take the 🖌 **Paint** tool (key 7), both kinds:

- **Vertex paint** — colour per corner, the cheap retro look: shade a corridor
  darker toward its end, warm a floor near a light, tint one wall.
- **Texture paint** — a resolution-independent overlay, so detail doesn't depend
  on how many faces the wall has. A dab paints in world space across faces, so a
  stroke down a wall-floor seam shades both surfaces at once: painted ambient
  occlusion, the baked retro look, with no bake.

Paint **follows the geometry through edits**. Every render vertex and triangle
carries a durable name — its face (identified by the set of corners it uses) plus
its corner or fan index — so moving a vertex, assigning a slot, cutting a face
elsewhere, or deleting one leaves every surviving surface's paint exactly where
it was. Only faces that genuinely changed (the top of an extrusion, the two
halves of a cut) come back unpainted, because they are new surfaces.

Two things worth knowing:

- **Undo takes the paint back too.** Every geometry edit banks the paint that was
  on the mesh alongside the shape, under those same durable names — so undoing an
  extrude brings back the face *and* the shading it was carrying, and redo puts it
  where it was again. Paint whenever you like; Ctrl+Z means what it says.
  (Anything you painted *after* the edit is never overwritten by a returning
  surface — live paint always wins.)
- Saved paint is checked against the geometry it was painted on. If a map mesh
  changed between the save and the load — a hand-edited `maps/*.ron` sidecar, a
  restored backup, a file recovered from version control without its partner —
  the editor **refuses** the stale paint and says so rather than scattering it
  across the wrong faces. In normal use you will never see it: a scene load
  adopts map geometry *before* paint, so the paint always has its triangulation
  to attach to.

## 6. Keys

Every control has one, and every one is rebindable in the Map panel's **KEYS**
section. Two rules keep them out of each other's way: map chords are only
consulted while the ⬢ Map tool is active and you aren't typing, and the keys the
editor keeps for itself (the fly camera, the tool digits, focus/grid/gizmo
toggles) are **refused** at bind time with the reason — so a broken binding can't
be created in the first place.

Defaults worth memorising: `E` extrude, `I` inset, `/` knife, `U` select all,
`Shift+U` none, `\` invert, `Tab` cycle mode, `X` cycle gizmo, `V` cycle handles.

## 7. Where the geometry lives

A `Matter::MapMesh { id }` node carries only its id; the geometry lives beside
the scene in `<project>/maps/<scene>.map.ron` and renders through the ordinary
mesh path (one part per material slot). That is why per-face materials, colliders,
shadows, vertex paint and custom shaders all work on a blockout without any of
them knowing what a map mesh is.

If that sidecar exists but can't be read, the editor says so, leaves the nodes
empty, and **refuses to save map geometry** for the session — a transient IO
error must never be able to replace a level with placeholder cubes and then
persist them.

See also: [the design proposal](map-tools-proposal.md),
[materials & textures](subsystems/materials-and-textures.md),
[the vertex-paint design](vertex-paint-proposal.md).
