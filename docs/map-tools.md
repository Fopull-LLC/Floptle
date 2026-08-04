# Building levels in the editor — the ▦ Map tool

Blockout geometry you draw, cut, texture and **paint** without leaving Floptle.
Map meshes are for LEVELS — walls, floors, stairs, arches, ramps; characters and
props still come from Blender ([asset pipeline](subsystems/asset-pipeline.md)).

Press **8** (or Window ▸ ▦ Map) to turn the tool on. The **▦ Map** panel is the
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

Growing a selection: **Grow** (one more ring), **Shrink** (take the outer ring
back off), **Connected** (the whole shell), **Coplanar** (the flat region a face
sits in), **Edge loop** (run an edge selection through its quad loops), and
**Select** on a material slot (every face wearing it).

**Warped faces** selects every face whose corners no longer sit in one plane.
After a few edits, a face that looks wrong is usually a warped one, and this is
how you find it without hunting by eye. Triangles are never warped, so they never
show up here.

### Right-click for what you can do

Right-clicking in the viewport lists the operations that apply to what you have
selected — extrude, inset, subdivide, bridge, flip, split off, delete, weld,
snap — plus the whole Select menu and the vertex/edge/face switch. Everything
that isn't applicable is greyed out rather than hidden, so the menu is also the
answer to "why can't I bridge these?" (bridge takes exactly two faces).

**Select through the surface** (off by default) decides whether sub-objects
hidden behind the mesh are clickable. Off means you stop grabbing the vertex on
the far side of a wall.

## 3. Shape it

- **⬆ Extrude** (`E`) pushes the selected faces out along their own normal —
  walls out of a floor. With grid snap on, it steps by the grid. Selecting
  *every* face of a closed shape has no direction to go, so it declines rather
  than guessing one.
- **⊡ Inset** (`I`) shrinks a copy of each face inside its own border. Inset then
  extrude carves a recess (a window, a doorway).
- **⊞ Subdivide**, **⇌ Bridge** (join two faces with a tube of walls),
  **🗑 Delete**, **✂ Split off** (the selection becomes its own node),
  **⇄ Flip** / **Flip all**, **⊙ Weld**, **⌗ Snap to grid**.
- The gizmo does **move / rotate / scale** (`X` cycles) with handles aligned to
  the **selection's own normal**, its node, or the world (`V` cycles). Normal is
  the default: a diagonal wall pushes straight out of itself in one drag.

### Reading the wireframe: what's near, what's behind

The overlay is drawn flat over the scene, so from v0.20.0 it carries two depth
cues — otherwise a box's far rim looks exactly like its near one and there is no
way to tell what a click is about to grab:

- **Distance** — edges and vertex dots fade and thin with how far away they are,
  normalised over the mesh's own extent (so a doorframe and a hangar read the
  same way).
- **Behind the surface** — anything the mesh's own front faces hide draws faint,
  and a vertex round the back draws as a small **ring** instead of a filled dot.

Occluded geometry is still drawn and still **selectable** — being able to reach
through a blockout is the point of the see-through selection modes. The cue only
tells you which side of the shape you are looking at. Selected elements keep
their full brightness wherever they are, so a selection never disappears into
the fade.

When **nothing** faces you — standing inside a room, or looking at the back of a
one-sided plane — nothing is in the way, so the whole wireframe draws at full
strength. "All of it is behind the surface" is true there and useless.

### ✂ Knife

`/` arms the knife. Click one **edge or corner** of a face, then another on the
same face, and the face splits along that line. The point under the cursor is
shown live — a filled dot for a new corner mid-edge, a ring for an existing one,
so you can see which you're about to get.

**The first click chooses the face; after that the aim is locked to it.** The
second point is solved against that face's own plane, so the cursor can drift
past an edge, or over a face that is nearer the camera, and the cut still tracks
where you mean. (Before v0.20.0 every click re-picked the face from whatever the
ray hit first, so aiming near a corner landed the second point on the
neighbouring face and the tool quietly threw the cut away and started over.
Which face won depended on the camera angle — which is why turning around and
trying from the other side sometimes appeared to fix it.)

The cut carries into the faces that share those edges: they gain the same corner,
so the seam stays welded and no hairline crack opens along it. Both halves keep
the face's material slot.

After a cut the knife **keeps going** from the corner it just made, so a groove
can be walked across a face (or from one half into the next) in one gesture. Esc
ends the cut; Esc again puts the knife away.

A cut that can't divide a face — two points on one edge, two corners already
joined — **greys out while you are still aiming** and says why next to the
cursor, so you never click and wonder what happened. The preview asks the cut
itself, so what it shows and what the click does can never disagree.

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

Map meshes take the ✏ **Paint** tool (key 7), both kinds:

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
consulted while the ▦ Map tool is active and you aren't typing, and the keys the
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
