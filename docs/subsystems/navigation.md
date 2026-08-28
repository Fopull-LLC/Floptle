# Floptle — Navigation (`floptle-nav`)

Where a character can stand, how it gets from here to there, and the thing that
actually walks it.

> The scripting surface: [`../lua-api.md`](../lua-api.md) (`nav.*`).
> The follow-along version: [`../tutorials/pathfinding.md`](../tutorials/pathfinding.md).
> Where this sits in the engine: [`../ARCHITECTURE.md`](../ARCHITECTURE.md).

`floptle-nav` depends on nothing but `serde`. No GPU, no ECS, no egui: a bake is
triangles and numbers, so every claim on this page can be tested by writing down
a floor and asserting what comes back. The editor half — which nodes count as
ground, where the file goes — lives in `floptle-editor/src/nav_bake.rs`, and the
scripting half in `floptle-script/src/nav_api.rs`.

## The two halves

Most navigation systems are described as one thing and are really two, and it is
worth naming them separately because they fail differently.

**The navmesh** answers questions. Where can this character stand? What is the
route from here to there? How far is it to *walk*? Is that point even reachable?
It is baked once from the level and is then a read-only structure that hundreds
of things can ask at once.

**The agent layer** walks. It holds a route, notices when it has gone stale,
steps along it at the right speed, cuts corners when it can see round them,
avoids its neighbours, crosses a ladder, slows down at the end, and says
*blocked* rather than standing still in silence.

The second half is the one every project ends up rewriting badly. It ships here.

## Baking

Triangles in, convex polygons out, through four stages:

1. **Heightfield.** The world is divided into square columns and every triangle
   claims the columns it *overlaps* — not the columns its centre falls in. A
   vertical wall has zero area in plan, so centre-sampling was structurally
   blind to walls, and the fix is the whole of `heightfield.rs`. Each column ends
   up with a sorted stack of **solids**: a top, a bottom, and whether the top is
   flat enough to stand on.
2. **Walkable grid.** Cells within `step_height` of each other are the same
   surface (that is what makes a staircase one place and a ledge two); then every
   cell within `agent_radius` of an edge is **eroded** away; then what survives is
   flood-filled into **regions**. Erosion happens *before* grouping, deliberately:
   a doorway narrower than the character has to disappear before anything decides
   the two rooms are connected through it.
3. **Polygons.** Each region is cut greedily into large axis-aligned
   **rectangles**. Rectangles are convex by construction and share whole edges,
   which is exactly what a funnel wants. A diagonal wall comes out stair-stepped
   — more polygons than strictly needed, hidden by the smoothing.
4. **Portals.** Two rectangles that share a run of cell edges are linked, and the
   link carries that shared segment, stored left-and-right *as seen walking
   through it*, once per direction.
5. **Links.** Every ledge the flood fill stopped at is probed outward for ground
   to drop onto or jump to, and each one that joins two otherwise separate
   pieces becomes an off-mesh link. See [Links the bake finds for
   itself](#links-the-bake-finds-for-itself).

Between 2 and 3 there is a **prune**: any island of walkable ground smaller than
`min_region_area` square metres is thrown away. A real level bakes hundreds of
specks — the top of a crate, a window sill, the triangle of floor behind a pillar
— and every one of them is an island nothing can reach, a patch in the overlay,
and somewhere `nearest` can strand a character that asked to be put on the
navmesh. The **largest** island is never pruned, however small it is, so a bake
of a tiny level is still a bake of that level.

### The four numbers

`agent_radius`, `agent_height`, `max_slope`, `step_height` — Unity's four, in the
same words, because they are the four that actually describe walking and nobody
should have to learn a second vocabulary for the same idea.

`max_drop` and `max_jump` are the two that decide what the bake joins up for
itself, and `min_region_area` is what it throws away. They are covered under
[Links the bake finds for itself](#links-the-bake-finds-for-itself) and in the
prune above.

`cell_size` is the fifth and it is the performance knob: halving it quadruples
the bake. It also has to stay small next to the radius, because erosion happens
in whole columns and rounds **up** — at a 0.25 cell and a 0.1 radius every edge
loses 0.25 m rather than 0.1, and a corridor two cells wide vanishes. The bake
says so, in the Inspector, with the number to use.

### The box has to cover the level

The volume's box is the bake's extent, and a box smaller than the level produces
**a navmesh of one corner of the map that looks exactly like a navmesh of the
map**: it bakes cleanly, reports a healthy polygon count, draws a convincing
overlay, and characters walk on it right up to an invisible edge where they stop.
Nothing downstream can tell the difference — this is the same silent-plausible-
result shape as the unreadable `.fnav`, and it is worse, because the result is
not even wrong.

Two things answer it. `auto_bounds` ("fit the box to what it finds") is on for a
new volume and measures the geometry rather than trusting a number, which is why
it is the default. And when the box *is* sized by hand, the bake compares it
against what it was given and says both figures out loud — "the volume covers
24 × 32 m of a level that spans 846 × 538 m, so 12,300 of 12,900 triangles were
left out of it" — in the Console and in the Inspector, where somebody looks when
a character will not walk somewhere. It only speaks when the gap is real: a level
always spills a little past a box drawn round it, and a warning that fires on
every bake is off by the second week.

### What counts as ground

Anything a character would **collide** with: the collidable switch, a static
rigidbody, or terrain. That is the rule that stays right as a level changes — a
wall built today blocks a path today, without anybody remembering to tag it.
Three things take a node back out, in order: `NavMeshExclude`, the layer filter,
and being switched off.

The engine does not guess from node type. A model made in the ▦ Model tab is not
assumed to be level geometry any more than a mesh is; the developer's collidable
switch is the statement of intent, and inferring one would mean a class of asset
that cannot be used the way its author wanted.

## Queries

All of these come in a plain form and a `_with(filter)` form, and all of them are
`nav.*` in Lua.

| | |
|---|---|
| `nearest` | the closest walkable point, and which polygon it is on |
| `path` | A\* over the polygons, then a funnel — see below |
| `reachable` | yes or no, without building the route |
| `distance` | how far it is to **walk**, which is the number a decision should be made on |
| `raycast` | walk a straight line and report where it stops — the walker's answer, not the collider's |
| `random_point` | a point on the surface, weighted by area, from two numbers **the caller supplies** |
| `region_at` | which island, so an impossible search can be ruled out with one integer compare |

`nearest` is the query everything else starts with, so it is indexed: a uniform
bucket grid over the polygons in plan (`index.rs`). Without it, every query cost
a scan over every polygon in the level, which is fine for one character and
quadratic for a crowd — 200 units against a 4,000-polygon level is 800,000
rectangle tests a frame, and it gets worse as the level grows rather than as the
army does. A scaling guard asserts that stepping a fixed crowd on a level four
times the size costs about the same.

### The funnel

A\* gives a **corridor** — the polygons to cross. Walking their centres is the
look of pathfinding rather than the look of walking, so the corridor is pulled
taut with the classic simple stupid funnel: carry a left and a right edge out
from the last corner, narrow them as the portals allow, and the moment they cross
each other the one they crossed over was a corner. One pass, no smoothing
afterwards, and no path that cuts a wall.

### Two kinds of failure

An end that is **not on the navmesh at all** answers `None` — the question was
about a place this mesh does not cover. A goal that is on the mesh but **cut off**
answers a real route to the closest reachable point, with `complete: false`
beside it. A character that walks to the near side of a chasm and stops is
behaving; one that stands still because the answer was empty looks broken.

The agent layer keeps the two apart rather than folding both into "blocked":
`agent.offMesh` is true when the order named a place the navmesh does not
describe. They look identical from outside — a character standing still — and
have nothing in common. One is a level question answered by walking somewhere
else; the other is a bake question answered by a volume that covers the ground
you are pointing at, and no amount of re-ordering will touch it.

## Areas and filters

An **area** is a label painted on the ground at bake time by a Nav Area volume —
*water*, *mud*, *road*, *danger* — carrying a default cost, because "mud is slow"
is usually a fact about the mud. A volume can also **carve**, which removes the
ground from the bake entirely: the answer to "nothing walks here" that does not
involve an invisible wall nobody remembers building.

A **filter** is one character's reading of those labels: a multiplier per area on
top of the level's, and a bit per area saying whether this character will set
foot in it. Filters live in the query, so a guard who takes the road and a zombie
who wades the river share one bake and get different routes.

Areas are matched **by name**, never by index, all the way from the volume to the
Lua table — inserting an area in the editor cannot silently re-point a script at
a different one. Up to 32 kinds of ground, because the include/exclude set is one
`u32`.

A discount is carried into A\*'s estimate (`filter.cheapest`). Straight-line
distance stops being admissible the moment some ground costs *less* than ordinary
ground, and an inadmissible heuristic quietly returns worse routes than the ones
it walked past.

`nav.ground()` lists them — `{ {name, cost}… }` — and every rectangle from
`nav.areas()` carries a one-based index into it. Both were added because "areas
are matched by name" is only true if a script can find out what the names *are*;
without the list a filter is written by guessing, and a guess that misses reads
as "nothing to avoid" rather than as an error.

## Links

A navmesh is a surface, so it can only say "walk along the floor to there".
Everything a character does that is not walking — dropping off a ledge, climbing
a ladder, vaulting, stepping through a door — is a **Nav Link**: two points, a
cost in metres of ordinary walking, a direction, and a switch.

A link is deliberately dumb. It does not know what a ladder is; what makes it one
is the animation a script plays while an agent reports crossing it. `agent.link`
is the link's name for the whole traversal and `agent.linkProgress` runs 0 to 1,
so an animation driven from it cannot disagree with the movement.

**`duration` is how long crossing takes, in seconds.** Zero means "at walking
speed", which is right for a vault and wrong for a lift — a slow platform wants
its own number, and `linkProgress` then runs at that rate rather than at the
speed the character walks. The pace is fixed when the crossing starts, so a
`speed` change mid-climb cannot make an animation drift out of step with it.

**Cost and duration are different questions.** Cost is what the *router* pays to
consider the link, in metres of ordinary walking; duration is what the
*character* spends crossing it. A teleporter is cheap and instant, a long ladder
is cheap to choose and slow to climb, and a guarded door can be made expensive
without becoming slow.

`nav.link(name, false)` shuts one. Every route that used it repaths, in the same
frame, with nothing rebaked — and anybody already halfway across finishes
crossing, because halfway up a ladder is the one state nothing downstream can
recover from.

`nav.offLinks()` reads them all back as data — `{ id, name, from, to,
bidirectional, cost, duration, enabled, ground, kind, generated }`, world space. **Careful with
the two things called links:** that one is the handful an author placed;
`nav.links()` is the thousands of portals the bake derived between neighbouring
rectangles. The names are inherited from both genuinely being links, and are
worth checking before reading either.

**An end that lands on nothing leaves the link unresolved rather than dropped**,
and the bake names it in the Console. A door that quietly does nothing is exactly
the silent-failure shape this engine's own audit found again and again: the level
looks right, the route goes the long way, and nothing anywhere says why.

### Links the bake finds for itself

Placing a link at every ledge is not an answer. A room has dozens, a level has
hundreds, and every one of them moves the moment somebody moves the geometry. So
the bake works out two kinds itself, from two numbers on the Nav Mesh node:

| | | |
|---|---|---|
| **drop height** (`max_drop`) | ground more than `step_height` below and no further down than this | a **one-way** drop, because falling is |
| **jump distance** (`max_jump`) | ground within `step_height` either way and no further across than this | a **two-way** jump, because a gap you can clear you can clear coming back |

Set either to `0` to switch that kind off. With both off, every ledge in the
level is a wall and the only ways across are the Nav Link nodes you place.

Each generated link carries a `kind` of `"drop"` or `"jump"`; a Nav Link node is
`"placed"`. A character refuses a whole kind in the query rather than by needing
its own bake — `nav.agent(node, { filter = { canJump = false } })` — which is
what a turret, a cart or a chained dog wants.

**Jump distance is measured between the two edges of the walkable surface**,
which is the distance the overlay draws — *not* the width of the hole in the
floor. Erosion has already pulled the surface back by `agent_radius` at each
side, so a metre of real chasm is a two-metre gap in the navmesh to a
half-metre-wide character, and any jump distance under `2 × agent_radius` can
never fire at all. This was found by looking at a rendered picture of a level
whose chasm produced no jumps, which is why the default is 2 m rather than the
1 m that reads as reasonable and does nothing.

Four rules keep the count sane and the result honest:

- **Only where walking cannot already get there.** A candidate whose two ends are
  in the same region is thrown away — the two being in one region *means* there
  is already a way to walk between them, so the link would be a shortcut. A
  level's ledges offer tens of thousands of shortcuts and the router pays for
  every edge on every query for the rest of the game.
- **A wall is not a gap.** The probe stops the moment a column has something
  standing up in it taller than the ledge, so two rooms either side of a partition
  are never joined by a hop over it.
- **A carve is not a gap either.** Ground carved out with a blocking Nav Area is
  the designer saying *nothing may walk here*; a bake that answered that by
  inventing a way over the top would be a tool overruling the person using it.
- **Spacing.** A twenty-metre balcony would otherwise get a drop per cell. Once
  one is taken, nothing within a body's width or so of its mouth is taken again,
  and a two-way jump suppresses the far bank looking back — so a chasm is one
  crossing rather than two.

There is a ceiling of 4096 generated links per bake. It is not a guess at what is
reasonable; it is a guard against the level that would otherwise hand the router
a hundred thousand edges. A bake that reaches it says so, with what to change.

### Regions, and islands

They are different numbers and the difference matters.

A **region** is what the walking surface flooded into, so a level with one ledge
in it is two regions whether or not anything can get down. An **island** counts
the links in — `NavMesh::islands()` — so it is what a character can actually
reach. The Inspector's *"N separate areas"* is the second one, alongside how much
of the walkable ground the biggest island holds: *"494 separate areas"* is a
number nobody can act on, and *"the largest holds 3% of the ground"* says the
bake has shattered and points at the settings that did it.

Connectivity is read either way round, so a one-way drop counts as joining two
pieces. Whether you can get back out is a different question.

### On a level that builds itself, the navmesh arrives late

**There is no navmesh in `start()` on a procedural level**, because there is no
geometry yet for a bake to measure. `nav.ready()` is false, and the obvious shape

```lua
function start(node)
    if nav.ready() then self.agent = nav.agent(node) end   -- never runs
end
```

leaves `self.agent` nil for the whole session, so every routing call falls
through to whatever the script does without one. Nothing errors. It looks exactly
like broken pathfinding.

Ask for it every frame until you have one. The reason this bites is that the
correct handling of *"no navmesh yet"* and the incorrect handling of *"no navmesh
ever"* are the same code.

## Reading it from the editor

An editor package gets the same `nav` table, minus everything that moves —
[`docs/editor-scripting.md`](../editor-scripting.md). A level-analysis tool wants
the shape of the walkable surface and nothing to do with driving anything around
it, and there is no simulation running for an agent or an obstacle to act on
anyway.

It reads the **editor's** bake and not the running game's, which during Play may
have obstacles carved out of it. Two mirrors of one navmesh drift, so there is
one publish point (`Editor::publish_nav_mesh`) and both readers are fed from it.

## Runtime obstacles

```lua
local crate = nav.obstacle(node.position, vec3(1, 2, 1))
-- …later
crate:remove()
```

A crate put down in a corridor blocks it. `nav.obstacle(centre, size)` cuts that
box out of the baked surface in place: the polygons it covers are split around
it, the portals through them are re-derived, and everything walking a route that
went that way repaths. `remove()` gives the ground back.

**It is an option, not a replacement.** The background rebake below is still
what is always right, and it is the honest answer when a level has genuinely
changed shape — a building came down, a bridge extended. Carving answers the
narrower question, *something is standing here now*, and it earns its place by
being much cheaper at it:

| level  | polygons | full rebake | carve   | remove  |
| ------ | -------- | ----------- | ------- | ------- |
| 32 m   |      186 |    6.5 ms   | 0.011 ms | 0.005 ms |
| 64 m   |      686 |   25.0 ms   | 0.037 ms | 0.017 ms |
| 128 m  |    2 646 |  108.3 ms   | 0.159 ms | 0.059 ms |
| 256 m  |   10 406 |  459.7 ms   | 0.618 ms | 0.238 ms |

`cargo run --release -p floptle-nav --example carve_probe` prints that table, and
it is the acceptance test rather than a nice-to-have: if the gap ever closes,
carving should go and the rebake should do the work.

**Every change re-derives the whole carved mesh from a pristine copy of the
bake.** That sounds wasteful and is what makes it trustworthy. Removal is exact
— taking the last obstacle away hands back the bake itself, so the same query
answers what it answered before, to the bit — and two crates in one doorway are
two boxes subtracted from one rectangle in one pass, with no order to get wrong.
The expensive half of a bake, voxelising the world and flood-filling it, never
runs.

**A hole is snapped outward to whole navmesh cells.** Polygons carry grid
columns as well as world bounds, and an arbitrary box would leave the two
disagreeing. Outward is the direction to be wrong in: a crate blocking slightly
more than its footprint is a crate, one blocking slightly less is a character
walking through it. Read `obstacle.size` for what was actually cut.

**Regions are recomputed where a carve bit.** A wall dropped across the only
corridor splits an island in two, and `nav.reachable` answers from the region
ids before it searches — so ids a carve has invalidated would be a lie it
repeats. Only the islands that were touched are renumbered, so nothing else in
the level changes colour in the overlay.

**Nothing here is written to disk**, for the same reason a bake taken during
Play is not: pressing Stop has to give the level back.

There is deliberately no *moving* obstacle. Carving every frame is rebuilding
every frame, which is the cost this exists to avoid — something that moves
wants an agent, or an area filter, or a rebake.

## Re-measuring one box of it

A level that builds itself changes a **box** at a time, and re-measuring the
whole level to account for one 32 m chunk is the wrong unit of work by
construction: the amount of *new* level per chunk is constant, so the cost per
chunk should be too, and instead it grows with how much is loaded.

`nav.rebake(centre, size)` measures the box and splices the answer into the mesh
in hand. It queues, like `spawn` — re-measuring needs the world's triangles and
the scripting side does not have them — and lands in the same pass, after that
pass's spawns and destroys, so a chunk can build itself and ask in one breath.

| | one 32 m chunk | the whole ring |
|---|---|---|
| 2×2 chunks | 2.8 ms | 4.1 ms |
| 3×3 chunks | 2.2 ms | 8.1 ms |
| 4×4 chunks | 2.6 ms | 14.0 ms |

The finding is the shape of the second column, not the first.

### The seam is the whole problem

Two navmeshes do not join because they are adjacent. A portal exists between two
rectangles that **share an edge**, and the surviving polygons end wherever the
old bake put them — not on the box's boundary. So the host's polygons are **cut**
at the box, by the same subtraction a carve uses, and the incoming ones are
clipped to it. Both sides then end on the same coordinate to the last bit, and
the bake's own portal rule finds the joins with nothing special about the seam.

That is also why the two bakes do **not** have to be on the same grid. Only the
two edges at the seam have to agree, and clipping both to one number is what
makes them.

### It is not carving

`nav.obstacle` takes ground away and can put it back, because the box it cut is a
thing standing on a level that has not changed. A splice is the opposite claim —
*the level here is different now* — so it writes through to the bake and becomes
what carves are re-derived from. **Obstacles outstanding at the time survive it**:
re-measuring the ground under a crate does not move the crate.

### Regions are re-flooded, not re-split

A splice can **merge** islands as readily as break them — a new corridor joins
two wings that were separate — and `resplit_regions` only splits. So a splice
floods the whole mesh and lets each island claim the lowest id it already
contained. It is O(polygons + links), which a splice can afford, and an island
nothing happened to keeps its number: the overlay does not recolour the level and
an id a script wrote down still means what it meant.

## Keeping it

A bake is written to `<scene>.<id>.fnav` beside the scene and loaded with it. It
is a build artefact measured in hundreds of kilobytes, so it does not go in the
`.ron` — a scene file is a thing people read and diff.

**The file carries a format version, and a reader that does not recognise one
says so by name.** Postcard is compact and not self-describing, which means
`#[serde(default)]` buys nothing: adding one field to the mesh changes the byte
layout and every older file stops parsing. That happened — v0.60 added areas and
links — and the reader of the day swallowed the error and reported "no bake",
which is indistinguishable from never having baked. A level's bake vanishing on
reopen while the editor says nothing is the worst failure this file has, so now
it names the file and says what happened to it. The Inspector shows which file
the bake in hand came from, so the question is answerable without reading a log.

**And then it makes it again.** A bake is a function of the level, the level is
open, and telling somebody to press a button to recompute something the editor
could recompute itself turns a format change into an evening. So an unreadable
or damaged `.fnav` starts a background bake a frame after the scene loads, saves
it, and says so once — work nobody asked for has to explain itself. Not a
*missing* file: a scene nobody has ever baked stays unbaked, because putting a
navmesh in a project that never asked for one is a decision that is not the
engine's to make.

### Static by default, automatic on request

`auto_rebake` is **off** by default, because most levels are finished: the bake
is on disk, it loads with the scene, and baking it again every session is work
nobody asked for.

Turn it on — while building a level, or in a game that puts buildings down as it
runs — and the volume hashes what a bake would see (the character's settings,
and every node the filter selects, in the pose it is in), waits for that to stop
changing, and bakes **on a worker thread**. The wait matters: dragging a wall
across a room would otherwise start a bake per frame, each wrong by the time it
landed. A hash rather than a dirty flag, because the question is "does the bake
in hand still describe this level" — undo, a reload and a nudge that ends where
it started all leave a flag set and the answer unchanged.

The gather stays on the main thread (it reads the world and imports models off
disk); what crosses the wall is voxelising, eroding and cutting, which is where
the time goes.

**A bake made while the game is running is not written to disk and does not
touch the node.** It describes what that session spawned or knocked down, and
persisting it would leave the project holding a navmesh nobody authored. Stop
gives the level back exactly as it was.

## The agent layer

```lua
function start(node)
  agent = nav.agent(node, { speed = 6 })
end

function update(node, dt)
  if ordered then agent:moveTo(where) end
end
```

There is no step function to call. The whole crowd is walked once a frame by the
script host, **after** every script's `update` and **before** the writes reach
the ECS — so an order given this frame is acted on this frame, and an agent's
movement rides the same pass as a hand-written one.

- **Following, not marching.** Each step an agent tries to see the corner *after*
  the one it is heading for, and skips ahead when it can. Paths bend where the
  level bends and straighten everywhere else, continuously, as the agent is
  pushed around.
- **Avoidance is sampled.** Given the velocity it wants, an agent scores a fan of
  alternatives by how soon each would run into somebody. Deliberately not ORCA:
  ORCA is exact for the program it solves and hard to reason about when it
  deadlocks, while a fan of samples degrades into "everybody slows down and
  shuffles", which is what a crowd in a doorway should look like anyway.
- **Separation is positional.** An overlap that has already happened has to be
  undone, and a velocity nudge only stops it getting worse. The push is applied
  first and the navmesh has the last word, so a shove can never put a character
  through a wall.
- **Searches are budgeted.** Agents queue for a route, oldest first, and
  `nav.budget()` of them are served per frame (8 by default). A hundred units
  given one order cost the same frame as one unit. Nobody stops walking while
  they wait.
- **Handles are salted.** An `AgentId` carries a generation, so a handle to a
  removed agent is detectably stale rather than quietly pointing at whoever took
  the slot — the same reason Detour salts a polygon reference.
- **Blocked is a state, not a silence.** Unreachable, or no measurable progress
  for `giveUpAfter` seconds. A unit standing still with no explanation is the
  single most common "the pathfinding is broken" report there is. It is not
  always terminal: a unit pinned by its own crowd (a doorway, a hundred friends
  at one waypoint) rests and tries again by itself, while one whose goal is
  genuinely cut off stays blocked until the level or the order changes.
- **Arrival is contagious on the last leg.** Ordering sixty units to one point
  can only ever put one of them on it; touching somebody who has already
  arrived at the same order counts as arriving. Without that the outer ring
  grinds against its own friends forever.

### What an agent is responsible for

Its position, and nothing else. It does not know what a node is or whether there
is a rigidbody involved. `drive` decides what its output means:

| | |
|---|---|
| `auto` (default) | a node with a physics body is steered through the body; one without has its transform moved |
| `transform` | move the transform, and let the navmesh be the collision |
| `velocity` | write the body's horizontal velocity and leave gravity, slopes and jumps to the sim |
| `none` | steer only — `agent.velocity` is yours to spend on a vehicle, a boat, an animation |

Positions are read back from the scene **every frame** rather than owned: whatever
else moved a node — a script, the sim, a parent, a cutscene — is the truth. An
agent that insisted otherwise would fight it and win, which is the ugliest way
for this to fail.

## What this does not do yet

Said plainly, because a list of features with the gaps left out is how somebody
finds out the hard way.

- **No tiling.** A bake is one grid over the whole level, so a rebake is the
  whole level. That is fine at room and building scale and it is what streaming
  and parallel bakes would need first.
- **A carved obstacle is a box, and it does not move.** `nav.obstacle` cuts an
  axis-aligned box out of the bake and `remove()` puts it back; there is no
  rotated or shaped obstacle, and nothing follows a node. A thing that moves
  wants an agent or a rebake, because carving per frame is rebuilding per frame.
- **A carve does not survive a rebake.** The two are independent: a background
  rebake replaces the mesh, and the holes cut in the one it replaced go with it.
  A game that does both has to cut its obstacles again afterwards.
- **Agents are not rolled back.** They step on the frame clock, not the tick
  clock, so a rollback-netcode game should drive movement itself and use the
  queries rather than the agent layer.
- **A very thick solid with inconsistent winding** can leave a stranded walkable
  patch inside it — unreachable, and harmless to a path, but it shows in the
  overlay.
- **Generated links are found along the four axis directions only**, so a ledge
  running diagonally is probed from its stair-stepped cells rather than square-on
  to its own edge. The links come out slightly off perpendicular; none are
  missed.
- **A generated link cannot be edited.** It belongs to the bake and a rebake
  makes it again. Turn one off with `nav.link(name, false)` for the session, or
  place a Nav Link and lower the drop height if you want the decision by hand.
- **Re-measuring one box does not find new drops or jumps in it.**
  `nav.rebake(box)` splices fresh ground in and re-resolves the links that were
  already there — an end that lost its floor comes back unresolved, and one that
  gained floor snaps onto it — but it does not probe the new ledges. A streamed
  level that wants links inside a chunk needs a full rebake for now.
