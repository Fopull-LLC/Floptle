# Build a 3D platformer

Run, jump, ride a moving platform, collect coins, reach the goal.

**some coding** · about 45 minutes · 11 steps

The finished project is a starter template: create a new project with the **platformer** template (in the Hub, or `floptle --new <dir> --template platformer`) to read the answer.

> Follow this along **inside the editor** — the 🎓 Learn tab has the same steps and ticks each one off as your project starts to match it.

A complete small platformer: a character you steer, a camera that follows,
platforms that move and carry you, coins that count, and a goal that ends it.

Six short scripts, none longer than about thirty lines. What you're really
learning is the shape they make together — a node script that only knows about
its own node, and one manager that knows about the game.

## What you should already know

The **First steps** tutorial, or equivalent: what a node is, how to attach a
script, and what `update` and `params` are.

## The plan

1. Ground to stand on, and a player with a physics body.
2. Movement and jumping, from named actions.
3. A camera that follows without making anyone seasick.
4. A moving platform you can ride.
5. Coins, a score, and a goal — via one manager script.

## 1. Make the ground

Start from a new empty project, or clear the starter scene — either is fine.

Add a cube (Hierarchy → **✚ New** → **■ Cube**), rename it `Ground`, and in
the Inspector set its **scale** to about `40, 1, 40`. That's a wide, thin slab.

Now give it a body: in the Inspector, **➕ Add Component → Rigidbody**, and set
its **mode** to **Static**. Static means "solid, and it never moves" — the
right answer for level geometry. The physics engine can skip it entirely except
when something touches it, which is why a level made of static bodies costs
almost nothing.

Drop a couple more cubes around as platforms while you're here. Same treatment:
scale them flat, Rigidbody, mode Static.

*Done when: a node called Ground is in the scene.*

## 2. Make the player

Add a **Capsule**, rename it `Player`, and lift it a few metres up so it starts
in the air (**position** y around 3).

Give it a **Rigidbody** and leave its **mode** on **Dynamic** — it should fall,
get pushed, and push back. Then set:

- **shape**: Capsule. A capsule is the standard character shape because it
  slides over small steps and bumps instead of catching on every edge a box
  would.
- **freeze rot**: turn on **x**, **y** and **z**. Without this your character
  face-plants the moment it touches anything, because a physics body has every
  right to tip over. Staying upright is a decision you make, not something
  physics does for you.

Press Play. It should fall and land. That's all you want from it right now.

*Done when: a node called Player is in the scene.*

## 3. Movement and jumping

Create the script below and attach it to `Player`.

### Why fixedUpdate and not update

`update` runs once per drawn frame, so it runs more often on a fast machine.
`fixedUpdate` runs exactly sixty times a second no matter what, and it is the
same clock physics steps on. Put anything that decides where things go in
`fixedUpdate` and your jump is the same height for everyone — put it in `update`
and it quietly isn't.

Rule of thumb: **gameplay in `fixedUpdate`, cameras in `lateUpdate`, everything
cosmetic in `update`.**

### Why we keep the vertical speed

    local vy = node.vy
    ...
    node.vel = vec3(x * params.speed, vy, -y * params.speed)

Gravity is the physics engine's job. If we wrote a `0` in there instead of `vy`,
we'd be overwriting gravity's work sixty times a second and the character would
float. So: read what physics decided vertically, keep it, replace the two
horizontal axes with what the player asked for.

### The forgiving ground check

`node.grounded` is true when the body is genuinely resting on something. It also
flickers off for a frame or two when you run down a slope — and a jump that
doesn't fire because of a flicker feels broken in a way players notice and can't
explain. So we also probe with a short `raycast` straight down and accept either
answer.

`scripts/platformerPlayer.lua`

```lua
-- Walks and jumps. Attach to a node with a CAPSULE Rigidbody, rotation frozen.
--
-- Every control is a NAMED ACTION from Settings → Input, so this works on a
-- keyboard and on a gamepad without knowing which one is plugged in.

defaults = {
  --@header Movement
  --@range 0 20 --@units m/s
  speed = 6.0,
  --@range 0 25 --@units m/s
  jump = 8.5,
  --@header Grounding
  --@desc How far below the feet to look for ground.
  --@range 0 4 --@units m
  groundRay = 1.2,
}

-- Gameplay belongs on the fixed tick: 60 Hz whatever the frame rate is doing,
-- so the jump is the same height on every machine.
function fixedUpdate(node, dt)
  -- Already deadzoned and clamped, so there is nothing left to tidy up.
  local x, y = input.axis2("Move")

  -- The body owns falling. Keep its vertical speed; replace the rest.
  local vy = node.vy

  -- Grounded, forgivingly: the contact flag, or a short probe downwards.
  local grounded = node.grounded
  if not grounded then
    grounded = raycast(node.pos, vec3(0, -1, 0), params.groundRay) ~= nil
  end

  if grounded and input.justPressed("Jump") then
    vy = params.jump
  end

  -- Forward is -Z, so pushing forward on the stick moves that way.
  node.vel = vec3(x * params.speed, vy, -y * params.speed)
end
```

*Done when: Player runs platformerPlayer.lua.*

## 4. Press Play and walk around

**W A S D** to move, **Space** to jump.

It works, and it's unpleasant — the camera is still the free-fly one from the
starter scene, so you're steering a character you have to chase manually. That's
the next step.

### Tune it while it runs

With `Player` selected and the game playing, drag `speed` and `jump` in the
Inspector. Find values you like before you write another line. Being able to do
this is most of why `defaults` exists, and it is much faster than reasoning
about what 8.5 metres per second ought to feel like.

*Done when: you've pressed Play.*

## 5. A camera that follows

Select the `Camera` node. Remove the `freelook` script it came with (the **…**
beside it → **🗑 Remove**), then create and attach the one below.

Now wire it up: with `Camera` selected, the Inspector shows a `target` row with a
node picker. Drag `Player` from the Hierarchy onto it.

### Why a noderef instead of find("Player")

    defaults = { target = noderef() }

`find("Player")` searches the scene by name every time you call it, and the day
you rename the node to `Hero` it silently returns nothing — no error, just a
camera that stopped working. A `noderef` is wired in the Inspector, survives
renames, and shows you at a glance what's connected to what.

### Why lateUpdate

`lateUpdate` runs after physics has finished moving everything for this frame.
A camera that reads the player's position in `update` gets *last* frame's
position — a lag of one frame that reads as jitter, and gets worse the faster
the player is moving.

### Why ease and not lerp

    node.pos = ease(node.pos, want, params.smoothing, dt)

The usual `pos = lerp(pos, want, 0.1)` moves a tenth of the remaining distance
*per frame*, so the camera is stiffer at 240 fps than at 60. `ease` takes `dt`
and covers a rate you specify per **second** — identical on every machine.

`scripts/platformerCamera.lua`

```lua
-- Follows a node from behind and above.
--
-- Wire `target` by dragging the Player onto the Inspector row: a reference
-- survives renames, and shows you what is connected to what.

defaults = {
  target = noderef(),
  --@range 0 30 --@units m
  height = 7.0,
  --@range 0 30 --@units m
  distance = 10.0,
  --@desc Higher catches up faster. Frame-rate independent either way.
  --@range 0 20
  smoothing = 6.0,
}

-- AFTER physics, so this samples where the player ended up this frame rather
-- than where it was last frame.
function lateUpdate(node, dt)
  local target = params.target
  if not target or not target.valid then return end

  local want = target.pos + vec3(0, params.height, params.distance)
  node.pos = ease(node.pos, want, params.smoothing, dt)
  node:lookAt(target.pos)
end
```

*Done when: Camera runs platformerCamera.lua.*

## 6. A platform that moves — and carries you

Add a **Cube**, name it `Platform`, scale it to something like `4, 0.5, 4`, and
put it out over the edge of the ground somewhere interesting.

Give it a **Rigidbody** with **mode** set to **Kinematic**, then attach the
script below.

### The three body modes, in one line each

- **Static** — solid, never moves. Level geometry.
- **Dynamic** — pushed by gravity and everything else. Players, crates, debris.
- **Kinematic** — you move it, physics doesn't, but it still pushes dynamic
  bodies out of the way. Moving platforms, lifts, doors.

Kinematic is what makes riding work. A static platform can't move; a dynamic one
would sag and get knocked around by the player standing on it. A kinematic one
goes exactly where the script puts it and carries what's on top.

Press Play and jump onto it.

`scripts/platformMover.lua`

```lua
-- Slides between where it starts and start + (dx, dy, dz), forever.
--
-- The node wants a Rigidbody in KINEMATIC mode: it never falls, and it carries
-- dynamic bodies standing on it, so the player rides along.

defaults = {
  --@header Travel
  --@units m
  dx = 0.0,
  dy = 0.0,
  dz = 6.0,
  --@desc Round trips per second.
  --@range 0 2
  speed = 0.25,
}

local from

function start(node)
  -- Where it was placed in the editor IS the start of the journey, so the
  -- script never needs to be told where it lives.
  from = node.pos
end

function update(node, dt)
  local to = from + vec3(params.dx, params.dy, params.dz)
  -- sin() gives -1..1; this maps it to 0..1, so the platform eases into each
  -- end of its travel instead of slamming into it.
  local t = (math.sin(time * params.speed * math.pi * 2) + 1) * 0.5
  node.pos = from:lerp(to, t)
end
```

*Done when: Platform runs platformMover.lua.*

## 7. The manager: score, respawn, HUD

Add an **Empty** node, name it `Game`, and attach the script below.

### Why a manager

The score doesn't belong to any coin, and "you fell off the world" isn't the
player's business either. Both are facts about the *game*. Putting them on one
node means there is exactly one place to look when the score is wrong, and coins
never need to know how many other coins there are.

Other scripts reach it like this:

    local game = findScript("platformerGame")
    if game then game.collect() end

`findScript` returns a handle to the first script of that kind anywhere in the
scene. Reading `game.collect` gets the function; note the **dot**, not a colon —
these are plain functions, not methods, so there's no `self` to pass.

### Why the functions aren't local

    function collect()

A `local function` is private to its file. These have to be reachable from
outside, so they're declared without `local`. That's the convention for anything
a manager publishes.

### The HUD is one call

`draw.text` puts a string on the screen in pixels, with no UI tree to build.
It's immediate mode: it draws for one frame, so it lives in `update` and is
re-issued every frame you want it visible. For a score counter that's exactly
right. (When you want buttons and layout, that's what the **◫ UI** tab is for.)

`scripts/platformerGame.lua`

```lua
-- The script that knows about the GAME rather than about a node: the score,
-- falling off the world, and whether you have won.
--
-- Put it on an Empty node. Everything else reaches it with
-- findScript("platformerGame") and calls the functions below.

defaults = {
  --@desc Fall below this height and you are put back at the start.
  --@units m
  fallY = -20,
  --@desc Where the start is.
  --@units m
  spawnY = 3.0,
}

local score = 0
local won = false
local player

function start(node)
  player = find("Player")
  score = 0
  won = false
end

-- Called by coin.lua. Not `local`, so a script handle can reach it.
function collect()
  score = score + 1
end

-- Called by goal.lua.
function reach()
  won = true
end

function update(node, dt)
  -- Respawn instead of falling forever. Zero the velocity too, or you arrive
  -- back at the top already travelling at terminal velocity.
  if player and player.valid and player.y < params.fallY then
    player.pos = vec3(0, params.spawnY, 0)
    player.vel = vec3(0, 0, 0)
  end

  if not camera.exists() then return end
  local w, h = camera.screenSize()

  draw.text(24, 24, "Coins: " .. score, 24, 1, 0.85, 0.35)
  if won then
    draw.text(w * 0.5, h * 0.5, "You made it!", 44, 1, 1, 1, 1, "center")
  end
end
```

*Done when: Game runs platformerGame.lua.*

## 8. Tag the player

Select `Player`. In the Inspector, find the **tags** row and add the tag
`player`.

### What tags are for

The coin is about to ask "is the thing that touched me the player?". It could
compare names — `other.name == "Player"` — but that breaks the moment you add a
second player, or rename the node, or spawn one from a prefab with a suffix.

A tag is a label you can put on any number of nodes and test cheaply:
`other:hasTag("player")`. Group membership, not identity. Use tags for
"what kind of thing is this" and names for "which one is it".

*Done when: Player is tagged "player".*

## 9. Coins

Add a **Sphere**, name it `Coin`, shrink it (**scale** around `0.4`), and float it
somewhere the player has to jump for.

Give it a **Rigidbody**, set its **mode** to **Kinematic**, and tick
**trigger**.

### What a trigger is

A trigger has a shape and reports what enters it, but doesn't block anything —
you walk straight through. That is exactly what a pickup is. The same switch
turns a wall into a doorway you get told about, which is how checkpoints,
damage zones and level exits all work.

Attach `coin` and press Play. Walk into it: it vanishes and the counter goes up.

Once one works, select it, **Ctrl+D** to duplicate, and scatter a dozen. Every
copy shares the one script.

`scripts/coin.lua`

```lua
-- A pickup. The node needs a collider with `trigger` ticked: a trigger reports
-- what walks through it instead of blocking it.

function onTriggerEnter(node, other, hit)
  -- Anything can wander into this. Only the player collects it.
  if not other:hasTag("player") then return end

  local game = findScript("platformerGame")
  if game then game.collect() end

  -- Removes the node and its whole subtree. Queued until the end of the pass,
  -- so it is safe to call on yourself in the middle of a hook.
  node:destroy()
end
```

*Done when: scripts/coin.lua exists.*

## 10. The goal

Add one more node — a **Cube** works — name it `Goal`, put it at the end of your
level, give it a **Rigidbody** with **mode** **Kinematic** and **trigger** ticked, and
attach the script below.

Notice how little there is to it. It's `coin.lua` with one word changed. Once
you have a manager and triggers, most of "the game part" of a game is a trigger
that calls one function.

Press Play and finish your level.

`scripts/goal.lua`

```lua
-- The end of the level: the same shape as coin.lua, a different consequence.

function onTriggerEnter(node, other, hit)
  if not other:hasTag("player") then return end

  local game = findScript("platformerGame")
  if game then game.reach() end
end
```

*Done when: a node called Goal is in the scene.*

## 11. Make it yours

You have a platformer. Now make it a *level* — the fastest way to learn what any
of these numbers do is to build something you actually want to get to the end
of.

### Things worth trying next, roughly in order of effort

- **Coyote time.** Remember the last moment `grounded` was true and allow a jump
  for a fifth of a second after leaving a ledge. Nearly every platformer you
  have ever enjoyed does this, and nobody notices except by its absence.
- **Variable jump height.** Cut `vy` in half when the player releases Jump while
  still rising.
- **A model instead of a capsule.** Drop a `.glb` into Assets, parent it under
  `Player`, and turn the capsule's rendering off. If it's rigged, give it an
  Animation Controller and drive it from the body's speed — `thirdPerson.lua`
  in the shipped scripts does exactly this and is worth reading.
- **Real level geometry.** The **▦ Model** tab builds proper blockout geometry
  with proper collision, which beats a hundred stretched cubes.
- **Sound.** A coin with no sound is half a coin. `node:sound()` and the
  **≣ Mixer** tab.

### The finished version

Everything above is in the `platformer` starter template — create a project with
it from the Hub to compare notes.

