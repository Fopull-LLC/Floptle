# Tilemaps

Everything for building a 2D level: the **◫ Tiles** tab, tilesets (what each tile
*is*), autotiling, collision, and the Lua surface a game reads it all through.

New here? The three-minute version:

1. Add a **▦ Tilemap** node (◫ Tiles ⏵ *Add layer*, or the Hierarchy's Add menu).
2. Give it a spritesheet in the Inspector — a texture, and `sheetCols` /
   `sheetRows` so the engine knows how it is cut.
3. Press **9** for the tile tool and paint in the ⌖ Scene view.
4. For collision or autotiling, press *New tileset* in the ◫ Tiles tab and tick
   what you want on each tile in the palette.

---

## 1. A layer is a node

There is no layer list to learn. **A tile layer is a `Matter::Tilemap` node**, so
it already has a transform, a material, a name, a visibility flag and a place in
the Hierarchy — and everything that moves, hides, renames, parents, duplicates or
deletes a layer is the ordinary node operation you already know.

The ◫ Tiles tab's layer list is a *view* of the scene's tilemap nodes. Which means
the Hierarchy and the Tiles tab can never disagree about whether a layer is
showing, because there is only one switch.

Parallax and depth come from the transform: *Add layer* puts each new layer 0.1
units in front of the last. (Not at the same Z — two layers on one plane make the
depth test pick arbitrarily between them and the map shimmers as the camera
moves.)

## 2. The sheet is the material

A tilemap draws from its node's ordinary `Material`: the `texture`, and
`sheetCols` × `sheetRows` for how it is cut. So a tilemap is dressed exactly like
every other surface — same texture picker, same filtering, same custom `.flsl`
shader if you want one — and a project does not learn a second way to say "this
texture, chopped this way".

Cell indices are **row-major from the top-left of the sheet**, starting at 0.

New layers are created **unlit**, because a 2D layer lit by the scene's sun is a
2D layer that goes dark at night.

### One mesh, no seams

The whole grid is **one mesh, one draw call**. That is not only a performance
choice. A tilemap built from one quad per tile has a hairline of background
between tiles that opens and closes as the camera moves: each quad's edge is
computed through its own transform, so two touching edges land either side of a
pixel boundary independently. Here every tile's corners come from the same
expression, so a shared edge is *bit-identical* on both sides and there is no gap
to show through — at any zoom, from any camera position.

## 3. Painting

Press **9** (or pick ◫ in the viewport tool strip) and paint in the ⌖ Scene view.
The ◫ Tiles tab opens with it.

| Tool | Key | What it does |
|---|---|---|
| ✏ Brush | **B** | Paint the current tile; drag for a stroke |
| ✖ Erase | **E** | Clear squares; drag to erase a stroke |
| ▬ Rectangle | **R** | Drag a filled rectangle |
| ▭ Frame | **F** | Drag a rectangle *outline* — a room's walls in one gesture |
| ╱ Line | **L** | Drag a straight line |
| ◍ Fill | **G** | Flood-fill the connected region under the pointer |
| ◉ Pick | **I** | Take the square under the pointer as the current tile |
| ▢ Select | **S** | Drag out a rectangle to copy, move, turn or clear |
| ✚ Move | **M** | Drag the selection somewhere else |

Those letters are the tile tool's **while it is held**. Two of them (F, G) are the
editor's frame-selection and grid toggle everywhere else; switching tools hands
them straight back, and the Tiles tab has its own Grid checkbox.

**Undo is ordinary undo.** A tilemap's squares are scene state, so a stroke is one
Ctrl-Z — the same Ctrl-Z as everything else, with no separate tile history to get
out of step with the scene's.

**The fill is four-connected**, so it does not leak through a diagonal wall.

### Multi-tile brushes

Drag a rectangle in the palette and the brush becomes that whole block. Painting
places all of it at once — a whole doorway, a whole tree.

Select ▢ a region of the map, press **Copy**, then **Paste**: the clipboard
becomes the brush, so the next click puts it exactly where you aimed. (Pasting
straight into the map at a remembered position is the version that needs an undo
to correct.)

### Turning and mirroring

`↻` `⇔` `⇕` turn the **selection** when there is one, and the **brush** when there
is not. Same buttons, because "turn this" is one idea.

A square tile has exactly **eight** placements: four rotations, each optionally
mirrored. That is why the engine stores an orientation as `(rot, flipX)` rather
than three independent booleans — a vertical flip is not independent, it is a
horizontal flip composed with a half-turn. `flipY` is still accepted everywhere
you might write it; it composes to the same eight states.

Rotating a **multi-tile** stamp turns both the layout *and* each tile in it. (Only
turning the layout is the bug that makes a rotated pipe corner point the wrong
way — invisible until the art is directional, which is exactly when you want to
rotate it.)

The orientation rides in the tile value itself, in bits above the cell index. A
map written before orientations existed is a list of bare indices and means
exactly what it always did.

## 4. Tilesets — what a tile *is*

A **tileset** says what each cell of a spritesheet means: whether it collides,
what it is tagged, which autotile shapes it draws, whether it animates. It lives at
`<project>/tilesets/<name>.tileset.ron` and is shared by every layer cut from that
sheet.

Press *New tileset* in the ◫ Tiles tab and it is made for the layer's own sheet.

### More than one sheet on one layer

A tileset can carry several images. **◫ Tiles ▸ TILESET ▸ SHEETS ▸ + sheet**
adds one; give it an image and its cols/rows, and the palette grows a tab per
sheet. Everything you place from any tab lands on the **same layer**.

That matters more than it sounds. Splitting a level across three tilemap nodes —
one per sheet — is not a workaround, it changes what the level is: a wall on one
node is not a neighbour of a wall on another, so nothing autotiles across the
join, the collision merge stops at it and leaves a seam, and every grid tool
(bucket, rectangle, stamp, retile) stops there too. One layer, several sheets,
and all of that keeps working across the whole map.

Drawing costs one call per sheet the layer actually uses — bounded by how many
images a level has, not by how many tiles.

**Adding art never renumbers what you already placed.** Each sheet owns a fixed
block of the cell index, so growing sheet 0 from 4×4 to 16×16 leaves every square
placed from sheet 1 meaning exactly what it did. Removing a sheet is offered only
for the last one, for the same reason.

A project that has never added a sheet is unaffected in every particular: there
is one implicit sheet, it is the layer's own material, and every index ever
written is on it.

**Why a tileset and not per-square data.** Solidity is a property of the ART. A
brick is solid everywhere a brick appears. Recording it per square means the
answer is stored hundreds of times, a level built before you decided bricks were
solid keeps the old answer, and marking one more tile solid becomes a job of
repainting the level rather than ticking a box. With a tileset, tick "solid" on
the brick and **every brick in every scene collides — including the ones already
placed.**

### The palette is the tileset editor

Click a tile in the palette and its properties are right underneath. The palette
also draws what it knows, over the art:

* a solid tile gets a **collision overlay in the shape of its collider** — so a
  half-tile collider looks like a half tile, not like a tick;
* an autotiled tile gets a **3×3 neighbourhood diagram** of a shape it draws,
  and `+2` beside it if it draws more than one;
* an animated tile gets a ▷.

### Collision

Four cases per tile: **none**, **full**, **half** (top / bottom / left / right),
or a hand-set **rect** in the unit tile.

The half is named in the tile's own art, so it **turns with the tile**: the bottom
half of a tile rotated a quarter-turn clockwise is its *left* half. (That is why
the side is stored rather than a `y = 0.5` — "the top half" survives a rotation
and a number does not.)

Select several tiles in the palette and press **All solid** to mark them in one
go, which is what makes a 256-tile sheet a minute's work.

There are deliberately no slopes. Static colliders in this engine are boxes,
spheres, capsules and meshes; offering a slope would mean drawing one and having
it behave as its bounding box, which is worse than not offering it.

### Colliders are merged

Solid squares become as few boxes as the shape allows. A 100×100 solid floor is
**one** box, not ten thousand.

That is not an optimisation, it is what makes tile collision usable. Ten thousand
static colliders is more than most whole 3D levels have, and the sim rebuilds its
broadphase index over all of them. And the second reason matters more in play: a
character sliding along a row of separate boxes **catches on the seams between
them** — each box's face is its own plane, and at a shallow angle the
depenetration pass ticks across each boundary. One merged box has no interior
seams. This is the classic 2D-platformer bug and merging is the classic fix.

Tick **Collision** in the tab to draw the merged boxes over the map. They are the
same boxes the sim gets, so what you see is what a character walks on.

The layer needs the **Collidable** switch on (Inspector ⏵ Collider), like any
other static geometry, and it uses the node's own physics **layer**.

### Tags

Free strings a script reads — `"ice"`, `"water"`, `"damage"`, `"ladder"`. This is
how a tilemap carries gameplay without the game keeping a second table keyed by
cell index, which goes stale the moment the artist reorders the sheet.

### Animated tiles

Give a tile a list of extra frames and a rate and every square using it animates
together — torches, water, conveyors. Frame 0 is the tile itself.

Animation is a *view* of the map: the frame is never written back to the grid, so
a saved scene does not record whichever moment you happened to hit Ctrl-S on. It
runs in the editor as well as in Play, so you can see a torch flicker while you
are placing torches.

The mesh rebuilds only when the frame it shows changes. A map with nothing
animated in it is never rebuilt at all.

## 5. Autotiling

An **autotile group** is a set of tiles that pick themselves by what is next to
them: paint a shape and the corners, edges and ends appear.

1. In ◫ Tiles ⏵ AUTOTILE, add a group — **Edges (16 tiles)** or **Blob (47
   tiles)**.
2. In its **RULES** grid, click a shape, then click the tile that draws it. The
   next empty shape arms itself, so filling a preset is one run of alternating
   clicks.
3. Click any of the group's tiles in the palette and paint.

If your sheet happens to be laid out in the preset's own order you can skip step
2: drag out the run of tiles and press **Assign preset to N selected**.

**Edges** uses the four orthogonal neighbours: a path, a wall run, a pipe network.
**Blob** uses all eight with the corner rule, which is what you need for terrain
that has inside corners as well as outside ones.

### One tile, many shapes — one shape, many tiles

Both directions are ordinary, and neither costs you anything.

**A tile can draw as many shapes as you like.** One plain fill square is usually
the answer to several neighbourhoods, and a sheet where the artist drew a single
inside corner rather than four wants that one tile in four shapes. Click the
shapes and click the same tile each time; nothing is moved off anything.

**A shape can hold as many tiles as you like.** They are *variants* — four grass
squares that all mean "surrounded" — and which one a square gets is decided by
where that square is, so a field varies without any two loads of the level
disagreeing. A shape with alternates is marked **×N** in the grid; arm it to see
them all, and click one to take it back off.

Listing the same tile twice makes it twice as likely, which is how you get a
rare flower without drawing nine plain squares.

The **next shape after the first tile** checkbox is what makes both work: it
advances only when a shape goes from empty to having something, so a second
click stays on the shape you are looking at. Turn it off to stay put entirely.

**Assign preset** understands variants too. Select a whole multiple of the
preset's length — 32 tiles for the 16-shape preset — and it reads them as
pass after pass, giving every shape two.

### About the presets

Every tool has a different idea of what order a 16- or 47-tile sheet is laid out
in, and none is more correct than the others. A preset that guesses your artist's
order and gets it wrong produces *plausible* wrongness — it tiles, it just tiles
with the wrong corners, and it reads as bad art rather than a wrong table.

So: the presets here are **ascending mask order**, stated rather than named after
a tool, and **every shape is drawn as a 3×3 diagram** — in the RULES grid, and
on the palette beside any tile that answers one. If a preset guessed wrong you
can see which tiles disagree, and pointing a shape at a different tile is two
clicks. That is what makes offering a guess safe.

Masks are one bit per neighbour, clockwise from north:

```
  NW  N  NE          128   1   2
   W  ·  E     =      64   ·   4
  SW  S  SE           32  16   8
```

North is up on screen, which for a tilemap means *row − 1* (the grid is row-major
from the top-left).

For a **Blob** group a corner bit only counts when both of its adjacent edges are
also set. Without that rule there are 256 combinations and an artist would have to
draw all of them; with it there are 47, and the ones that fall away are exactly
the ones that look identical anyway.

### Two rules worth knowing

**Off the map counts as filled.** A shape painted to the edge of the grid does not
grow a coastline against the border — the map ends there, it is not a hole. (Get
this wrong and every level's outer wall comes out edged.)

**A half-drawn group leaves holes, never erases.** If a group has no tile for a
neighbourhood, that square is left exactly as painted. The tab shows "12 of 47
drawn" so an incomplete group is visible rather than something you discover as a
gap in a level.

### Groups that meet

Put two groups in each other's **joins** and their tiles count as "the same stuff"
for masking — so grass and dirt can meet without either growing an edge against
the other. Joins are one-way by default (a cliff can edge against sky without sky
edging against the cliff); the tab sets both sides when you tick them.

### Retiling

*Retile as I paint* is on by default. **Retile all** fixes a whole layer after you
change the rules.

A retile always covers the painted region **plus a one-square ring** — painting a
square changes what its neighbours should draw, so anything narrower leaves a seam
of stale edge tiles exactly one square wide around every stroke.

A square you turned by hand keeps its orientation through a retile.

A square's **variant** is decided by where the square is, not by a random number,
so retiling the same map twice gives the same result — and so does opening it
tomorrow, or on another machine. A level that reshuffled its own grass on load
would be unusable.

## 6. The orthographic camera

Under perspective, a tilemap two units further back is drawn slightly smaller — so
a parallax layer changes *scale* as well as speed, and two layers cannot be lined
up. An orthographic camera draws everything at the same scale at every distance,
which is what 2D wants.

**In the game:** select a Camera node and set *projection* to **orthographic**, and
*height* to how many world units the shot covers top to bottom. (With 1-unit tiles
that is how many tiles tall it is; the width follows the viewport's aspect.)
`fovY` is unused when it is orthographic, and the Inspector hides it rather than
leaving a knob that does nothing.

From Lua:

```lua
find('Cam'):setCamera{ projection = 'orthographic', orthoHeight = 12 }
```

**In the editor:** ⌖ Scene ⏵ ▦ View ⏵ **Orthographic**. Picking a plane lock
(Front / Side / Top) turns it on as well, since squaring the view to a plane is
almost always the 2D intent — and it is still a separate checkbox, because an
orthographic *free* view is a real thing to want.

Under an orthographic view the wheel **zooms the view height** instead of moving
the camera. It has to: moving forward changes nothing you can see when the view is
the same size at every distance, so the wheel would simply appear dead. Panning
scales with the zoom, so a drag tracks the pointer at any height.

A camera gizmo draws a **box** for an orthographic camera rather than a pyramid —
the frame really is the same rectangle at every distance, and a gizmo that
contradicts the projection is worse than no gizmo, because people frame shots by
it.

Render targets have the projection too. A minimap is very often the one place a
perspective game wants an orthographic shot:

```lua
find('MapEye'):setCamera{
  target = 'minimap', width = 256, height = 256, hz = 10,
  projection = 'orthographic', orthoHeight = 80,
}
```

## 7. From Lua

```lua
function start(node)
  node:setTilemap{ cols = 40, rows = 22, tile = 1.0,
                   tileset = 'tilesets/cave.tileset.ron' }
  local tm = node:tilemap()
  tm:fill(-1)                       -- -1, nil and EMPTY_TILE all mean "empty"
  tm:fillRect(0, 20, 39, 21, 3)     -- a floor
  tm:set(5, 19, 7, { rot = 90 })    -- one turned tile
  tm:autotile(0, 0, 39, 21)         -- let the group pick its own corners
end
```

### The grid

| Call | |
|---|---|
| `tm:set(x, y, cell [, xform])` | One square, 0-based from the **top-left**. Outside the grid is a no-op, not a wrap. |
| `tm:get(x, y)` | The cell, orientation stripped — `nil` outside the grid and on an empty square. |
| `tm:at(x, y)` | `cell, rot, flipX` — the whole answer, for art that faces a direction. |
| `tm:fill(cell [, xform])` | Every square, including the empty ones. |
| `tm:fillRect(x0, y0, x1, y1, cell [, xform])` | Corners in either order, clipped to the grid. |
| `tm:size()` | `cols, rows`. |
| `tm:tileSize()` | The world edge length of one square. |
| `tm:resize{ cols =, rows =, offsetX =, offsetY = }` | Keeps whatever overlaps. `offsetY = 1` grows a row on **top**. |

`xform` is `{ rot = 0|90|180|270, flipX = bool, flipY = bool }`. `rot` is degrees
clockwise and must be a multiple of 90 — 45 is not one of the eight things a
square tile can be, and rounding it to 0 would place the tile unturned with
nothing said.

### World space

```lua
local x, y = tm:cellAt(player.worldPos)     -- which square is the player on?
if x and tm:hasTag(x, y + 1, 'ice') then ... end
local p = tm:worldAt(x, y)                  -- that square's centre, in the world
```

`cellAt` goes through the tilemap node's **own transform**, so a map that has been
moved, turned or scaled still answers correctly. This is the part worth using
rather than reimplementing: a Lua copy divides by a tile size and cannot see the
node's transform, so it is right until the day the map moves.

`worldAt` gives the **centre** of the square, because what you do with it is put
something on the tile.

### What a tile is

| Call | |
|---|---|
| `tm:solid(x, y)` | Does the tileset say that square collides? `false` with no tileset. |
| `tm:tags(x, y)` | Its tags, as a list. |
| `tm:hasTag(x, y, "ice")` | The common case, without a table per square — what a per-frame ground check should call. |
| `tm:tileset()` | The `.tileset.ron` path, or `nil`. |
| `tm:autotile(x0, y0, x1, y1)` | Recompute the region's autotiled squares (and the ring around it). |

Call `tm:autotile` after a **run** of `tm:set`, not per square: retiling per write
would be O(area) each time, and it would fight a stroke still being laid down.

## 8. What is not here yet

Written down because "it does not exist" beats "it exists and does nothing":

* **Tileset undo.** The scene's tiles undo with Ctrl-Z; edits to the *tileset*
  (collision, tags, groups) are saved to their own file and are not on the undo
  stack. Delete the file to start over.
* **Chunked / infinite maps.** A tilemap is `cols × rows`; a very large world is
  several tilemap nodes.
* **Slope collision.** See §4.
* **One-way platforms.** The physics layer has no one-way contact filter, so this
  would have to be built there first.
* **Importing `.tmx` / `.ldtk`.** No importer yet.

## See also

* [`scripting.md`](scripting.md) — the guided tour of the Lua surface
* [`lua-api.md`](lua-api.md) — every call, generated from the same table the
  editor's Docs tab and autocomplete read
* [`physics.md`](physics.md) — layers, the collision matrix, and the broadphase
  that merged colliders feed
