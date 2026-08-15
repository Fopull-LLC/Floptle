# Pathfinding — command a squad

Click the ground, and a dozen units find their own way there.

**some coding** · about 40 minutes · 9 steps

> Follow this along **inside the editor** — the 🎓 Learn tab has the same steps and ticks each one off as your project starts to match it.

Units that walk round walls instead of into them, through the door instead of
past it, and past each other instead of through each other. By the end you'll
have a level you can click around in, a squad that obeys, a ladder they can
climb, a patch of mud they'd rather avoid, and a gate you can shut on them.

## The one idea

**The navmesh is where a character can stand.** You bake it once from the level
you already built; from then on "walk to that door" is a search over a few
hundred shapes rather than a guess.

**An agent is a thing that walks it.** You make one with `nav.agent(node)`,
tell it `moveTo`, and it does the rest — the route, the corners, the slowing
down at the end, the going round its neighbours, and asking again when the
level changes. You never write a follow loop and you never call a step
function: the engine walks the whole crowd once a frame, after your `update`.

## What you need first

A level with some walls in it. The **▦ Model** tab is the quickest way to block
one out, but anything works — meshes, terrain, primitives — as long as it is
**collidable**, because that is what a bake looks for.

## 1. Build a room with a wall in it

Make a floor and two or three walls, with a doorway between them. A 20×20 plane
and some stretched cubes is plenty — this tutorial is about what walks on it,
not what it looks like.

The one thing that matters: **everything you want to block a path has to be
collidable.** Select each piece and tick **Collidable** in the Inspector. That
is the same switch physics uses, and it is deliberate — a wall that stops a
falling crate should stop a walking guard, without anybody remembering to tag
it twice.

Name the floor `Ground` so the next step has something to check.

### What is NOT baked

- Anything with **Navmesh Exclude** on it. A glass floor collides and is not
  somewhere to stand.
- Anything switched off.
- Anything outside the layers the navmesh node is filtered to (by default:
  none, which means everything).

*Done when: a node called Ground is in the scene.*

## 2. Add the Nav Mesh node and bake

**+ Add ▸ ⬚ Nav Mesh**. Name it `Navmesh`. Its Inspector is the character the
level is being measured for:

- **radius** — how wide they are. Ground closer than this to a wall or a drop
  is dropped, so a route can be walked by something with a body rather than by
  a point. This is why a path never scrapes a corner.
- **height** — how tall. Ground with less headroom than this is not walkable.
- **max slope** — the steepest floor they'll walk up.
- **step height** — the tallest lip they step over rather than walk around.
  This is what makes a staircase one place and a ledge two.
- **cell size** — how finely the level is sampled. The one performance knob:
  halving it quadruples the bake. Keep it under half the radius, and the
  Inspector will say so in orange when it isn't.

Press **⬚ Bake**. The Console says how many polygons over how many square
metres, and the Scene view draws the walkable surface over your level.

### Read the result before you trust it

- **The shape should look like the floor you can actually stand on** — pulled
  back from every wall by the radius, and absent under things you can't walk on.
- **"N separate areas"** in the Inspector means the level is in islands: a
  character cannot walk between them. Usually that's a doorway narrower than
  the character, and it's better to find out here than to find out from a unit
  that won't move.
- **Nothing baked at all?** The Inspector says how many objects the filter
  selects, before you bake. `0 objects` is a collidable problem, not a bake
  problem.

*Done when: a node called Navmesh is in the scene.*

## 3. One unit that walks where you click

Add a **Capsule**, name it `Unit`, and put it on the floor. Attach this script.

Three lines do the work: make an agent, give it a point, and read how it's
getting on. There is no follow loop here and there is not meant to be one.

### The click

`input.mouseRay()` gives the ray under the cursor and `raycast` finds where it
hits the world. `nav.nearest` then drops that point onto the walkable surface —
so clicking a wall sends the unit to the floor beside it rather than nowhere.

`scripts/navUnit.lua`

```lua
-- A unit that walks where you click.
--
-- SETUP: attach to anything you want to send places. The scene needs a
-- Nav Mesh node that has been baked, and a camera to click through.

defaults = {
  --@range 0 20 --@units m/s
  speed = 5.0,
  -- Close enough to the order to call it arrived. Keep it at least the
  -- unit's radius or a crowd jostles forever trying to stand on one spot.
  --@range 0.1 5 --@units m
  arrive = 0.6,
}

local agent

function start(node)
  -- One call. From here the engine walks this node: it works out the
  -- route, follows it, goes round the others, and slows down at the end.
  agent = nav.agent(node, { speed = params.speed, arrive = params.arrive })
end

function update(node, dt)
  if input.mousePressed(0) then
    local hit = raycast(input.mouseRay())
    if hit then
      -- Drop the click onto the walkable surface. Clicking a wall then
      -- sends the unit to the floor beside it instead of nowhere.
      local spot = nav.nearest(hit.point, 2.0)
      if spot then agent:moveTo(spot) end
    end
  end

  -- Draw the route it decided on. Delete this once you believe it.
  local corners = agent:corners()
  local from = node.worldPos
  for _, c in ipairs(corners) do
    draw.line(from.x, from.y + 0.1, from.z, c.x, c.y + 0.1, c.z, 0.4, 1.0, 0.6)
    from = c
  end
end
```

*Done when: Unit runs navUnit.lua.*

## 4. Press Play and click around

Click the far side of the wall. The unit should go **through the doorway**, not
into the wall — and the green line shows you the route it picked before it
walks it.

### When it doesn't

- **It doesn't move at all.** Either there's no bake (`nav.ready()` is false),
  or the click landed somewhere off the mesh. `nav.nearest` returning nil is
  the tell.
- **It walks a bit and stops.** That's `blocked`, and it's a real answer: the
  goal is on another island, or it's made no progress for a few seconds.
  `agent.state` says which.
- **It walks into a wall you built.** That wall isn't collidable, so the bake
  never saw it. Tick the box and bake again.

### The states, and what they're for

- `idle` — no order.
- `moving` — walking.
- `arrived` — got there. This is the flag to hang "and then attack" off.
- `blocked` — gave up, and it will tell you rather than standing there silently.
- `crossing` — on a link. The next step is about those.

*Done when: you've pressed Play.*

## 5. A squad, and why they don't merge

Duplicate the unit a dozen times (**Ctrl+D**), spread them out, and click. They
all go — and they go **round each other** rather than through.

That's `avoid` (on by default) and `separation`. Two things worth knowing:

- **`arrive` should be at least a unit's radius.** Ordered to one exact spot, a
  crowd that all think they haven't got there yet will push at each other
  forever.
- **`priority` decides who gives way.** A unit yields to anything of higher
  priority and expects anything lower to move for it. Equal priorities split
  the difference, which is what you want for a squad of the same thing.

### A hundred units and one order

They do not all think on the same frame. Agents queue for a route, oldest
first, and `nav.budget()` of them are served each frame (8 by default) — so a
big order costs the same frame as a small one. Nobody stands still waiting:
a unit with an old route keeps walking it.

Raise it with `nav.budget(32)` if you want a burst of orders acted on at once,
and lower it if searches ever show up in a frame graph.

## 6. A ladder they can climb

Build a ledge your units can't walk up to — a raised platform, or a second
floor. Bake again and you'll see two separate areas in the Inspector: the
navmesh is a surface, and it has no way to say "and you can climb here".

**+ Add ▸ ⇄ Nav Link** is how you say it. Put the node at the bottom of the
climb and drag the **far end** to the top. In the Inspector:

- **can be crossed both ways** — a ladder can. A jump down cannot, and making
  one two-way is a character walking up a cliff.
- **cost** — what crossing costs the router, in metres of ordinary walking.
  Raise it to make the link a last resort.
- **crossing takes** — seconds. 0 means at walking speed, which is right for a
  vault and wrong for a lift.

Name the node `Ladder`, then **bake again** — a link is joined up by the bake,
not while the game runs.

### If it does nothing

The Console will say so by name: *"nav link Ladder could not find the ground at
one end"*. Both mouths have to land somewhere a character could actually
stand — not inside the wall, not floating a metre above the floor.

### Playing the climb

While an agent is on a link, `agent.link` is that link's name and
`agent.linkProgress` runs 0 to 1. That's your animation hook:

```
if agent.link == "Ladder" then
  node:play("climb", { at = agent.linkProgress })
end
```

Driving the animation from `linkProgress` rather than from a timer means the
climb and the movement cannot disagree, however long the ladder is.

*Done when: a node called Ladder is in the scene.*

## 7. Mud they'd rather not cross

**+ Add ▸ ▨ Nav Area**, sized over a patch of your floor. Call the area `mud`
and leave the cost at 4. Bake again.

Now click across it. Routes go **round** the mud when there's a way round, and
straight through when there isn't — because a cost is a preference, not a wall.

### One level, different characters

The cost is the level's opinion. A character can have its own:

```
agent = nav.agent(node, {
  filter = {
    avoid = { "water" },      -- will not set foot in it
    cost  = { mud = 0.5 },    -- and rather likes mud
  },
})
```

That's how a guard who takes the road and a zombie who wades the river share
one bake. Areas are named, not numbered, so adding one in the editor can never
re-point a script at a different one.

### Or take the ground away entirely

Tick **carve this out of the navmesh** on the volume and nothing walks there at
all, whatever any character thinks about it. That's the answer to "keep out of
this room" that doesn't involve an invisible wall nobody remembers building.

## 8. A gate you can shut

Add a second Nav Link across a doorway and name it `Gate`. Now, from any script:

```
nav.link("Gate", false)   -- shut
nav.link("Gate", true)    -- open
nav.link("Gate")          -- is it open?
```

Shutting it makes every route that used it repath — nothing is rebaked, and it
happens in the same frame. A unit already halfway across **finishes crossing**
rather than stopping in mid-air, which is the one state nothing downstream
could recover from.

This is the whole mechanism behind doors, drawbridges, a rope somebody cuts,
and a bridge that burns down in act two.

## 9. Making it yours

What you have now is the whole loop: a level, a bake, agents that obey, and
three ways to change the level's mind at runtime. What to do next:

- **Follow a moving target.** `agent:moveTo(target.worldPos)` every frame is
  fine — re-ordering an agent to where it's already heading costs nothing.
- **Formations.** Give each unit a different point around the destination
  rather than all of them the same one, and they'll arrive as a group instead
  of a pile.
- **Wander and patrol.** `nav.random(math.random(), math.random(), here, 20)`
  picks a point on the walkable surface near somewhere.
- **Pick a target properly.** `nav.distance(a, b)` is how far it is to *walk*.
  "Chase the nearest one" built on straight-line distance picks the enemy on
  the other side of the wall, every time.
- **Drive something that isn't a capsule.** `drive = "none"` makes the agent
  steer without moving anything, and `agent.velocity` is yours to spend — on a
  vehicle with a turning circle, a boat, or an animation-driven character.
- **Two sizes of character.** A second Nav Mesh node with a bigger radius bakes
  a second surface; a bake belongs to the character it was measured for.

