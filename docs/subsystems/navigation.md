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

### The four numbers

`agent_radius`, `agent_height`, `max_slope`, `step_height` — Unity's four, in the
same words, because they are the four that actually describe walking and nobody
should have to learn a second vocabulary for the same idea.

`cell_size` is the fifth and it is the performance knob: halving it quadruples
the bake. It also has to stay small next to the radius, because erosion happens
in whole columns and rounds **up** — at a 0.25 cell and a 0.1 radius every edge
loses 0.25 m rather than 0.1, and a corridor two cells wide vanishes. The bake
says so, in the Inspector, with the number to use.

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

## Links

A navmesh is a surface, so it can only say "walk along the floor to there".
Everything a character does that is not walking — dropping off a ledge, climbing
a ladder, vaulting, stepping through a door — is a **Nav Link**: two points, a
cost in metres of ordinary walking, a direction, and a switch.

A link is deliberately dumb. It does not know what a ladder is; what makes it one
is the animation a script plays while an agent reports crossing it. `agent.link`
is the link's name for the whole traversal and `agent.linkProgress` runs 0 to 1,
so an animation driven from it cannot disagree with the movement.

`nav.link(name, false)` shuts one. Every route that used it repaths, in the same
frame, with nothing rebaked — and anybody already halfway across finishes
crossing, because halfway up a ladder is the one state nothing downstream can
recover from.

**An end that lands on nothing leaves the link unresolved rather than dropped**,
and the bake names it in the Console. A door that quietly does nothing is exactly
the silent-failure shape this engine's own audit found again and again: the level
looks right, the route goes the long way, and nothing anywhere says why.

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
- **No runtime obstacle carving.** A building placed during play does not appear
  in the navmesh until it is baked again. Links can be switched, areas cannot,
  and geometry cannot. For an RTS this is the gap worth knowing about.
- **The bake is synchronous.** Fractions of a second at the scales tested, and
  still a stall you would notice on a very large level.
- **Agents are not rolled back.** They step on the frame clock, not the tick
  clock, so a rollback-netcode game should drive movement itself and use the
  queries rather than the agent layer.
- **A very thick solid with inconsistent winding** can leave a stranded walkable
  patch inside it — unreachable, and harmless to a path, but it shows in the
  overlay.
