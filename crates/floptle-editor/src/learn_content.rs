//! The tutorials themselves — the text, the code, and what each step checks.
//!
//! Content, deliberately kept apart from [`crate::learn`]'s machinery: this file
//! is prose and Lua, and it should be editable by anyone who can write both
//! without reading a line of Rust.
//!
//! Three rules the whole file follows, because they are what makes the
//! difference between a tutorial and a wall of instructions:
//!
//! 1. **Every step says why.** "Put the camera code in `lateUpdate`" is a rule to
//!    memorise; "…because `update` reads where the player was last frame, and
//!    that lag is exactly what a jittery camera is" is something you can use
//!    again tomorrow on a problem this tutorial never mentioned.
//! 2. **Every script here is real.** They are compiled, linted and run by the
//!    tests at the bottom of this crate, and three of them are the actual
//!    contents of a starter template. Nothing is pseudo-code.
//! 3. **A step that can be checked, is.** See [`Check`].
//!
//! The Lua uses camelCase for anything the reader types or sees in the
//! Inspector, matching the rest of the scripting surface.

use crate::learn::{Check, Level, Step, Tutorial};

// The Lua every tutorial writes, named so that a starter template can ship the
// EXACT same file. One source, two consumers — a template whose scripts had
// quietly drifted from the tutorial that teaches them would be worse than no
// template at all.

/// `scripts/spinner.lua`
pub(crate) const SPINNER_LUA: &str = "\
-- Turns this node ninety degrees a second, forever.

function update(node, dt)
  node.yaw = node.yaw + math.rad(90) * dt
end";

/// `scripts/spinner.lua`
pub(crate) const SPINNER_2_LUA: &str = "\
-- Turns this node, at a speed you can change while the game is running.

defaults = {
  --@desc How fast it turns.
  --@range 0 720 --@units deg/s
  speed = 90,
}

function update(node, dt)
  node.yaw = node.yaw + math.rad(params.speed) * dt
end";

/// `scripts/spinner.lua`
pub(crate) const SPINNER_3_LUA: &str = "\
-- Turns, and drives around when you push a direction.

defaults = {
  --@desc How fast it turns.
  --@range 0 720 --@units deg/s
  speed = 90,
  --@desc How fast it moves when you push a direction.
  --@range 0 20 --@units m/s
  nudge = 3,
}

function update(node, dt)
  node.yaw = node.yaw + math.rad(params.speed) * dt

  -- x is left/right, y is back/forward: W A S D, or a gamepad stick.
  local x, y = input.axis2(\"Move\")
  node.x = node.x + x * params.nudge * dt
  node.z = node.z - y * params.nudge * dt
end";

/// `scripts/platformerPlayer.lua`
pub(crate) const PLATFORMER_PLAYER_LUA: &str = "\
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
  local x, y = input.axis2(\"Move\")

  -- The body owns falling. Keep its vertical speed; replace the rest.
  local vy = node.vy

  -- Grounded, forgivingly: the contact flag, or a short probe downwards.
  local grounded = node.grounded
  if not grounded then
    grounded = raycast(node.pos, vec3(0, -1, 0), params.groundRay) ~= nil
  end

  if grounded and input.justPressed(\"Jump\") then
    vy = params.jump
  end

  -- Forward is -Z, so pushing forward on the stick moves that way.
  node.vel = vec3(x * params.speed, vy, -y * params.speed)
end";

/// `scripts/platformerCamera.lua`
pub(crate) const PLATFORMER_CAMERA_LUA: &str = "\
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
end";

/// `scripts/platformMover.lua`
pub(crate) const PLATFORM_MOVER_LUA: &str = "\
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
end";

/// `scripts/platformerGame.lua`
pub(crate) const PLATFORMER_GAME_LUA: &str = "\
-- The script that knows about the GAME rather than about a node: the score,
-- falling off the world, and whether you have won.
--
-- Put it on an Empty node. Everything else reaches it with
-- findScript(\"platformerGame\") and calls the functions below.

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
  player = find(\"Player\")
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

  draw.text(24, 24, \"Coins: \" .. score, 24, 1, 0.85, 0.35)
  if won then
    draw.text(w * 0.5, h * 0.5, \"You made it!\", 44, 1, 1, 1, 1, \"center\")
  end
end";

/// `scripts/coin.lua`
pub(crate) const COIN_LUA: &str = "\
-- A pickup. The node needs a collider with `trigger` ticked: a trigger reports
-- what walks through it instead of blocking it.

function onTriggerEnter(node, other, hit)
  -- Anything can wander into this. Only the player collects it.
  if not other:hasTag(\"player\") then return end

  local game = findScript(\"platformerGame\")
  if game then game.collect() end

  -- Removes the node and its whole subtree. Queued until the end of the pass,
  -- so it is safe to call on yourself in the middle of a hook.
  node:destroy()
end";

/// `scripts/goal.lua`
pub(crate) const GOAL_LUA: &str = "\
-- The end of the level: the same shape as coin.lua, a different consequence.

function onTriggerEnter(node, other, hit)
  if not other:hasTag(\"player\") then return end

  local game = findScript(\"platformerGame\")
  if game then game.reach() end
end";

/// `scripts/topdownPlayer.lua`
pub(crate) const TOPDOWN_PLAYER_LUA: &str = "\
-- Eight-way movement on the ground plane, seen from above.

defaults = {
  --@range 0 20 --@units m/s
  speed = 5.0,
  --@desc Turn to face the way you are walking.
  faceTravel = true,
}

function fixedUpdate(node, dt)
  local x, y = input.axis2(\"Move\")

  -- World directions, not camera-relative: a top-down camera never turns, so
  -- up on the stick is north and stays north.
  local move = vec3(x, 0, -y)

  -- A diagonal is 1.41 long; without this, walking diagonally is faster.
  -- Only shorten it, so a half-pushed stick still walks at half speed.
  if move:length() > 1 then move = move:normalized() end

  -- Keep the vertical speed the body already has, so gravity still works.
  node.vel = vec3(move.x * params.speed, node.vy, move.z * params.speed)

  if params.faceTravel and move:length() > 0.01 then
    node.yaw = math.atan2(-move.x, -move.z)
  end
end";

/// `scripts/topdownCamera.lua`
pub(crate) const TOPDOWN_CAMERA_LUA: &str = "\
-- Sits above the target looking down, and never rotates.

defaults = {
  target = noderef(),
  --@range 2 40 --@units m
  height = 14.0,
  --@desc How far behind the target the view sits (0 = straight overhead).
  --@range -20 20 --@units m
  leadZ = 6.0,
  --@range 0 20
  smoothing = 8.0,
}

function lateUpdate(node, dt)
  local target = params.target
  if not target or not target.valid then return end

  local want = target.pos + vec3(0, params.height, params.leadZ)
  node.pos = ease(node.pos, want, params.smoothing, dt)
  node:lookAt(target.pos)
end";

/// `scripts/npcTalk.lua`
pub(crate) const NPC_TALK_LUA: &str = "\
-- Stand close, press Interact (E, or West on a pad), read the next line.
--
-- The lines are a string param split on \"|\", so one script covers every
-- villager in the game and each says something different.

defaults = {
  --@multiline
  --@desc Each line separated by a | character.
  lines = \"Hello, traveller.|The cave to the north is not safe.|Here, take this key.\",
  --@range 0 10 --@units m
  range = 2.5,
  --@desc Shown when you are close enough to talk.
  prompt = \"E — talk\",
}

local said = 0
local player
local dialogue = {}

function start(node)
  player = find(\"Player\")
  dialogue = {}
  -- Split on the separator. gmatch walks every run of not-a-pipe.
  for line in string.gmatch(params.lines, \"[^|]+\") do
    dialogue[#dialogue + 1] = line
  end
end

function update(node, dt)
  if not player or not player.valid then return end
  if not camera.exists() then return end

  if distance(node, player) > params.range then
    -- Walking away ends the conversation, so coming back starts it again.
    said = 0
    return
  end

  if input.justPressed(\"Interact\") then
    said = said + 1
    if said > #dialogue then said = 1 end
  end

  local w, h = camera.screenSize()
  if said == 0 then
    draw.text(w * 0.5, h - 90, params.prompt, 20, 1, 1, 1, 0.75, \"center\")
  else
    draw.text(w * 0.5, h - 90, dialogue[said], 24, 1, 1, 1, 1, \"center\")
  end
end";

/// `scripts/inventory.lua`
pub(crate) const INVENTORY_LUA: &str = "\
-- What the player is carrying, and the only script that knows it.
--
-- Pickups call add(); the door calls has(). One owner means one place to look
-- when the answer is wrong.

local items = {}

function start(node)
  -- save.* outlives scene loads, Stop, and quitting the editor — so what you
  -- picked up in the village is still yours inside the cave.
  items = save.get(\"items\", {})
end

function add(item)
  items[#items + 1] = item
  save.set(\"items\", items)
  log(\"picked up \" .. item)
end

function has(item)
  for _, held in ipairs(items) do
    if held == item then return true end
  end
  return false
end

function update(node, dt)
  if not camera.exists() then return end
  local w = camera.screenSize()
  draw.text(w - 24, 24, \"Carrying: \" .. #items, 20, 1, 1, 1, 0.8, \"right\")
end";

/// `scripts/itemPickup.lua`
pub(crate) const ITEM_PICKUP_LUA: &str = "\
-- Picked up by walking over it. The item's NAME is a param, so one script and
-- one prefab cover every item in the game.

defaults = {
  item = \"Rusty Key\",
}

function onTriggerEnter(node, other, hit)
  if not other:hasTag(\"player\") then return end

  local bag = findScript(\"inventory\")
  if bag then bag.add(params.item) end

  node:destroy()
end";

/// `scripts/door.lua`
pub(crate) const DOOR_LUA: &str = "\
-- A way into another scene, and a lock if you name a key.
--
-- scene.load swaps the whole world: every node goes, the new scene's arrive,
-- and every start() runs again. Anything that must survive lives in save.*
-- (which is why the inventory writes there).

defaults = {
  --@desc The scene to load — a file stem under scenes/.
  destination = \"cave\",
  --@desc Leave this empty for a door that is not locked.
  needs = \"Rusty Key\",
  --@multiline
  refusal = \"It's locked. Something about a key.\",
}

local blocked = false

function onTriggerEnter(node, other, hit)
  if not other:hasTag(\"player\") then return end

  if params.needs ~= \"\" then
    local bag = findScript(\"inventory\")
    if not bag or not bag.has(params.needs) then
      blocked = true
      -- Runs once, 2.5 seconds later, on the game clock — so it pauses when
      -- the game pauses, and there is no countdown to keep in update().
      after(2.5, function() blocked = false end)
      return
    end
  end

  scene.load(params.destination)
end

function update(node, dt)
  if not blocked then return end
  if not camera.exists() then return end

  local w, h = camera.screenSize()
  draw.text(w * 0.5, h * 0.5, params.refusal, 24, 1, 0.7, 0.6, 1, \"center\")
end";

/// `scripts/flappyBird.lua`
pub(crate) const FLAPPY_BIRD_LUA: &str = "\
-- One button. Press it and you go up; do nothing and gravity wins.

defaults = {
  --@desc Upward speed given by one flap.
  --@range 0 20 --@units m/s
  flap = 6.0,
  --@desc Tilt with the climb. Purely cosmetic.
  tilt = true,
}

function fixedUpdate(node, dt)
  -- Once it is over, stop responding — but keep falling, so the failure is
  -- something you watch happen rather than a freeze.
  local game = findScript(\"flappyGame\")
  if game and game.over then return end

  -- REPLACE the velocity rather than adding to it, so every flap is identical
  -- whatever you were already doing. That is what makes it learnable.
  if input.justPressed(\"Jump\") then
    node.vel = vec3(0, params.flap, 0)
  end

  -- The game is flat: nothing should ever leave z = 0.
  if math.abs(node.z) > 0.001 then
    node.pos = vec3(node.x, node.y, 0)
  end

  if params.tilt then
    node.pitch = math.clamp(node.vy * 0.08, -0.6, 0.6)
  end
end

function onCollisionEnter(node, other, hit)
  local game = findScript(\"flappyGame\")
  if game then game.lose() end
end";

/// `scripts/flappyPipe.lua`
pub(crate) const FLAPPY_PIPE_LUA: &str = "\
-- One obstacle: drifts toward the bird, scores as it passes, deletes itself
-- once it is safely off-screen.

defaults = {
  --@range 0 20 --@units m/s
  speed = 4.0,
  --@desc Removed once it is this far past the bird.
  --@range 0 40 --@units m
  behind = 12.0,
}

local scored = false

function update(node, dt)
  local game = findScript(\"flappyGame\")
  if game and game.over then return end

  node.x = node.x - params.speed * dt

  -- Score once, as it goes past. Without the flag this fires every frame the
  -- pipe spends left of zero.
  if not scored and node.x < 0 then
    scored = true
    if game then game.score() end
  end

  -- Each pipe tidies itself up, so the spawner never keeps a list.
  if node.x < -params.behind then
    node:destroy()
  end
end";

/// `scripts/flappyGame.lua`
pub(crate) const FLAPPY_GAME_LUA: &str = "\
-- The rules: spawn pipes, keep score, end the run, start it again.
--
-- Put this on an Empty node. It is the only script that knows the game is a
-- game — the bird just flaps, the pipes just drift.

defaults = {
  --@desc Prefab spawned as an obstacle.
  pipe = \"Pipe\",
  --@range 0.5 5 --@units s
  interval = 1.6,
  --@desc How far to the right pipes appear.
  --@units m
  spawnX = 14.0,
  --@header Gap height
  --@units m
  gapLow = 2.0,
  --@units m
  gapHigh = 6.5,
}

-- No `local`: this is the script's PUBLIC state. The bird and the pipes read
-- it through a script handle, and locals are private to their file.
over = false

local points = 0
local best = 0
local spawner

function start(node)
  over = false
  points = 0
  best = save.get(\"best\", 0)

  -- Repeats on the game clock and hands back a cancellable handle, so there is
  -- no countdown to keep in update() and it pauses when the game pauses.
  spawner = every(params.interval, function()
    local y = params.gapLow + math.random() * (params.gapHigh - params.gapLow)
    spawn(params.pipe, vec3(params.spawnX, y, 0))
  end)
end

function score()
  points = points + 1
end

function lose()
  if over then return end
  over = true
  -- Stop the pipes marching on over the game-over screen.
  if spawner then spawner:cancel() end
  if points > best then
    best = points
    save.set(\"best\", best)
  end
end

function update(node, dt)
  if not camera.exists() then return end
  local w, h = camera.screenSize()

  draw.text(w * 0.5, 40, tostring(points), 48, 1, 1, 1, 1, \"center\")

  if over then
    draw.text(w * 0.5, h * 0.5 - 20, \"Game over\", 40, 1, 0.5, 0.45, 1, \"center\")
    draw.text(w * 0.5, h * 0.5 + 30, \"Best \" .. best .. \" — Space to try again\",
      22, 1, 1, 1, 0.8, \"center\")

    -- Reloading the scene is a complete reset: every node back to how it was
    -- authored, every start() run again.
    if input.justPressed(\"Jump\") then
      scene.load(scene.current())
    end
  end
end";

/// `scripts/probe.lua`
pub(crate) const PROBE_LUA: &str = "\
-- A throwaway probe: watch the two clocks, and prove params are live.

defaults = {
  --@desc How often to report, in seconds.
  --@range 0.25 5 --@units s
  rate = 1.0,
}

-- File-scope locals are PER INSTANCE. Two nodes running this script keep two
-- separate pairs of counters.
local frames = 0
local ticks = 0

function start(node)
  log(node.name .. \": start\")

  -- The scheduler runs on the game clock, so this pauses when the game does.
  every(params.rate, function()
    log(string.format(\"%s: %d frames, %d ticks in the last %.1fs\",
      node.name, frames, ticks, params.rate))
    frames = 0
    ticks = 0
  end)
end

function update(node, dt)
  frames = frames + 1
end

-- Constant 60 Hz, whatever the frame rate is doing. This is the clock gameplay
-- belongs on.
function fixedUpdate(node, dt)
  ticks = ticks + 1
end";


pub(crate) const TUTORIALS: &[Tutorial] = &[
    FIRST_STEPS,
    PLATFORMER,
    TOPDOWN,
    FLAPPY,
    PATHFINDING,
    FOR_PROGRAMMERS,
];

// ---------------------------------------------------------------------------
// 5. Pathfinding — a squad that finds its own way
// ---------------------------------------------------------------------------

const PATHFINDING: Tutorial = Tutorial {
    id: "pathfinding",
    title: "Pathfinding — command a squad",
    tagline: "Click the ground, and a dozen units find their own way there.",
    level: Level::Intermediate,
    minutes: 40,
    template: None,
    intro: "\
Units that walk round walls instead of into them, through the door instead of
past it, and past each other instead of through each other. By the end you'll
have a level you can click around in, a squad that obeys, a ladder they can
climb, a patch of mud they'd rather avoid, and a gate you can shut on them.

## The one idea

**The navmesh is where a character can stand.** You bake it once from the level
you already built; from then on \"walk to that door\" is a search over a few
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
",
    steps: &[
        Step {
            title: "Build a room with a wall in it",
            body: "\
Make a floor and two or three walls, with a doorway between them. A 20×20 plane
and some stretched cubes is plenty — this tutorial is about what walks on it,
not what it looks like.

The one thing that matters: **everything you want to block a path has to be
collidable.** Select each piece and tick **Collidable** in the Inspector. That
is the same switch physics uses, and it is deliberate — a wall that stops a
falling crate should stop a walking guard, without anybody remembering to tag
it twice.

Name the floor `Ground` so the next step has something to check.

## What is NOT baked

- Anything with **Navmesh Exclude** on it. A glass floor collides and is not
  somewhere to stand.
- Anything switched off.
- Anything outside the layers the navmesh node is filtered to (by default:
  none, which means everything).
",
            code: None,
            check: Check::Node("Ground"),
        },
        Step {
            title: "Add the Nav Mesh node and bake",
            body: "\
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

## Read the result before you trust it

- **The shape should look like the floor you can actually stand on** — pulled
  back from every wall by the radius, and absent under things you can't walk on.
- **\"N separate areas\"** in the Inspector means the level is in islands: a
  character cannot walk between them. Usually that's a doorway narrower than
  the character, and it's better to find out here than to find out from a unit
  that won't move.
- **Nothing baked at all?** The Inspector says how many objects the filter
  selects, before you bake. `0 objects` is a collidable problem, not a bake
  problem.
",
            code: None,
            check: Check::Node("Navmesh"),
        },
        Step {
            title: "One unit that walks where you click",
            body: "\
Add a **Capsule**, name it `Unit`, and put it on the floor. Attach this script.

Three lines do the work: make an agent, give it a point, and read how it's
getting on. There is no follow loop here and there is not meant to be one.

## The click

`input.mouseRay()` gives the ray under the cursor and `raycast` finds where it
hits the world. `nav.nearest` then drops that point onto the walkable surface —
so clicking a wall sends the unit to the floor beside it rather than nowhere.
",
            code: Some((
                "navUnit",
                "-- A unit that walks where you click.\n\
                 --\n\
                 -- SETUP: attach to anything you want to send places. The scene needs a\n\
                 -- Nav Mesh node that has been baked, and a camera to click through.\n\
                 \n\
                 defaults = {\n\
                 \x20 --@range 0 20 --@units m/s\n\
                 \x20 speed = 5.0,\n\
                 \x20 -- Close enough to the order to call it arrived. Keep it at least the\n\
                 \x20 -- unit's radius or a crowd jostles forever trying to stand on one spot.\n\
                 \x20 --@range 0.1 5 --@units m\n\
                 \x20 arrive = 0.6,\n\
                 }\n\
                 \n\
                 local agent\n\
                 \n\
                 function start(node)\n\
                 \x20 -- One call. From here the engine walks this node: it works out the\n\
                 \x20 -- route, follows it, goes round the others, and slows down at the end.\n\
                 \x20 agent = nav.agent(node, { speed = params.speed, arrive = params.arrive })\n\
                 end\n\
                 \n\
                 function update(node, dt)\n\
                 \x20 if input.mousePressed(0) then\n\
                 \x20   local hit = raycast(input.mouseRay())\n\
                 \x20   if hit then\n\
                 \x20     -- Drop the click onto the walkable surface. Clicking a wall then\n\
                 \x20     -- sends the unit to the floor beside it instead of nowhere.\n\
                 \x20     local spot = nav.nearest(hit.point, 2.0)\n\
                 \x20     if spot then agent:moveTo(spot) end\n\
                 \x20   end\n\
                 \x20 end\n\
                 \n\
                 \x20 -- Draw the route it decided on. Delete this once you believe it.\n\
                 \x20 local corners = agent:corners()\n\
                 \x20 local from = node.worldPos\n\
                 \x20 for _, c in ipairs(corners) do\n\
                 \x20   draw.line(from.x, from.y + 0.1, from.z, c.x, c.y + 0.1, c.z, 0.4, 1.0, 0.6)\n\
                 \x20   from = c\n\
                 \x20 end\n\
                 end\n",
            )),
            check: Check::NodeRuns { node: "Unit", script: "navUnit" },
        },
        Step {
            title: "Press Play and click around",
            body: "\
Click the far side of the wall. The unit should go **through the doorway**, not
into the wall — and the green line shows you the route it picked before it
walks it.

## When it doesn't

- **It doesn't move at all.** Either there's no bake (`nav.ready()` is false),
  or the click landed somewhere off the mesh. `nav.nearest` returning nil is
  the tell.
- **It walks a bit and stops.** That's `blocked`, and it's a real answer: the
  goal is on another island, or it's made no progress for a few seconds.
  `agent.state` says which.
- **It walks into a wall you built.** That wall isn't collidable, so the bake
  never saw it. Tick the box and bake again.

## The states, and what they're for

- `idle` — no order.
- `moving` — walking.
- `arrived` — got there. This is the flag to hang \"and then attack\" off.
- `blocked` — gave up, and it will tell you rather than standing there silently.
- `crossing` — on a link. The next step is about those.
",
            code: None,
            check: Check::Played,
        },
        Step {
            title: "A squad, and why they don't merge",
            body: "\
Duplicate the unit a dozen times (**Ctrl+D**), spread them out, and click. They
all go — and they go **round each other** rather than through.

That's `avoid` (on by default) and `separation`. Two things worth knowing:

- **`arrive` should be at least a unit's radius.** Ordered to one exact spot, a
  crowd that all think they haven't got there yet will push at each other
  forever.
- **`priority` decides who gives way.** A unit yields to anything of higher
  priority and expects anything lower to move for it. Equal priorities split
  the difference, which is what you want for a squad of the same thing.

## A hundred units and one order

They do not all think on the same frame. Agents queue for a route, oldest
first, and `nav.budget()` of them are served each frame (8 by default) — so a
big order costs the same frame as a small one. Nobody stands still waiting:
a unit with an old route keeps walking it.

Raise it with `nav.budget(32)` if you want a burst of orders acted on at once,
and lower it if searches ever show up in a frame graph.
",
            code: None,
            check: Check::Read,
        },
        Step {
            title: "A ladder they can climb",
            body: "\
Build a ledge your units can't walk up to — a raised platform, or a second
floor. Bake again and you'll see two separate areas in the Inspector: the
navmesh is a surface, and it has no way to say \"and you can climb here\".

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

## If it does nothing

The Console will say so by name: *\"nav link Ladder could not find the ground at
one end\"*. Both mouths have to land somewhere a character could actually
stand — not inside the wall, not floating a metre above the floor.

## Playing the climb

While an agent is on a link, `agent.link` is that link's name and
`agent.linkProgress` runs 0 to 1. That's your animation hook:

```
if agent.link == \"Ladder\" then
  node:play(\"climb\", { at = agent.linkProgress })
end
```

Driving the animation from `linkProgress` rather than from a timer means the
climb and the movement cannot disagree, however long the ladder is.
",
            code: None,
            check: Check::Node("Ladder"),
        },
        Step {
            title: "Mud they'd rather not cross",
            body: "\
**+ Add ▸ ▨ Nav Area**, sized over a patch of your floor. Call the area `mud`
and leave the cost at 4. Bake again.

Now click across it. Routes go **round** the mud when there's a way round, and
straight through when there isn't — because a cost is a preference, not a wall.

## One level, different characters

The cost is the level's opinion. A character can have its own:

```
agent = nav.agent(node, {
  filter = {
    avoid = { \"water\" },      -- will not set foot in it
    cost  = { mud = 0.5 },    -- and rather likes mud
  },
})
```

That's how a guard who takes the road and a zombie who wades the river share
one bake. Areas are named, not numbered, so adding one in the editor can never
re-point a script at a different one.

## Or take the ground away entirely

Tick **carve this out of the navmesh** on the volume and nothing walks there at
all, whatever any character thinks about it. That's the answer to \"keep out of
this room\" that doesn't involve an invisible wall nobody remembers building.
",
            code: None,
            check: Check::Read,
        },
        Step {
            title: "A gate you can shut",
            body: "\
Add a second Nav Link across a doorway and name it `Gate`. Now, from any script:

```
nav.link(\"Gate\", false)   -- shut
nav.link(\"Gate\", true)    -- open
nav.link(\"Gate\")          -- is it open?
```

Shutting it makes every route that used it repath — nothing is rebaked, and it
happens in the same frame. A unit already halfway across **finishes crossing**
rather than stopping in mid-air, which is the one state nothing downstream
could recover from.

This is the whole mechanism behind doors, drawbridges, a rope somebody cuts,
and a bridge that burns down in act two.
",
            code: None,
            check: Check::Read,
        },
        Step {
            title: "Making it yours",
            body: "\
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
  \"Chase the nearest one\" built on straight-line distance picks the enemy on
  the other side of the wall, every time.
- **Drive something that isn't a capsule.** `drive = \"none\"` makes the agent
  steer without moving anything, and `agent.velocity` is yours to spend — on a
  vehicle with a turning circle, a boat, or an animation-driven character.
- **Two sizes of character.** A second Nav Mesh node with a bigger radius bakes
  a second surface; a bake belongs to the character it was measured for.
",
            code: None,
            check: Check::Read,
        },
    ],
};

// ---------------------------------------------------------------------------
// 1. First steps
// ---------------------------------------------------------------------------

const FIRST_STEPS: Tutorial = Tutorial {
    id: "first-steps",
    title: "First steps — make something move",
    tagline: "A cube you spin, tune and drive around, in about fifteen minutes.",
    level: Level::Beginner,
    minutes: 15,
    template: None,
    intro: "\
You don't need to know how to program to finish this. You need about fifteen
minutes and a willingness to press Play a lot.

By the end you'll have made a thing, given it a behaviour, changed that
behaviour without touching code, and driven it around with the keyboard. Those
four moves are most of what making a game is; everything else is more of them.

## The three words worth knowing first

- A **node** is a thing in your game. A cube, a camera, a light, the player.
  Everything in the Hierarchy panel is one.
- A **script** is a `.lua` text file describing a behaviour — spin, follow,
  explode. Scripts don't belong to any one node; you attach one to as many nodes
  as you like.
- **Play** runs your game. Scripts only run while you're playing. Press it
  again to stop, and everything goes back exactly how it was.
",
    steps: &[
        Step {
            title: "Look around",
            body: "\
Click into the **⌖ Scene** panel and hold the **right mouse button**. Now:

- **W A S D** — fly forward, left, back, right
- **mouse** — look
- **Space** — rise, **C** — drop

That's the editor's camera, not a game camera. It exists so you can get a look
at what you're building, and it has nothing to do with what a player will see.

Take thirty seconds to fly around the starter scene. The crate, ball and capsule
in front of you are ordinary nodes with physics on them — press **⏵ Play** and
they'll fall. Press it again to put them back.",
            code: None,
            check: Check::Read,
        },
        Step {
            title: "Add a cube and name it",
            body: "\
In the **Hierarchy** panel (top left), open the **✚ New** menu and pick
**■ Cube**. It appears at the origin.

Now rename it. Double-click its row in the Hierarchy, type `Spinner`, press
Enter.

## Why the name matters

It isn't decoration. Scripts find nodes by name — `find(\"Spinner\")` — and so
does this tutorial: the tick beside this step appears when a node called
`Spinner` exists. Get in the habit of naming things the moment you make them.
The alternative is a scene of eleven nodes called Cube.",
            code: None,
            check: Check::Node("Spinner"),
        },
        Step {
            title: "Write your first script",
            body: "\
Press **Create scripts/spinner.lua** below. That writes the file and opens it in
the **Scripting** tab.

## Reading it

`function update(node, dt)` declares a **hook** — a function the engine calls
for you. `update` runs once for every frame drawn, which on most machines is
somewhere between 60 and 240 times a second.

The engine hands it two things:

- `node` — the node this script is running on. Not \"the cube\"; whichever node
  it happens to be attached to. That's why one script can spin twenty things.
- `dt` — how many seconds the last frame took. A small number, around 0.016.

`node.yaw` is how far the node is turned around the vertical axis, in radians.
`math.rad(90)` is ninety degrees expressed in radians.

## The one line worth understanding properly

    node.yaw = node.yaw + math.rad(90) * dt

Multiplying by `dt` is what makes it ninety degrees **per second** rather than
ninety degrees **per frame**. Without it, the cube spins nearly four times
faster on a 240 Hz monitor than a 60 Hz one — the classic bug that makes a game
feel different on someone else's computer. Any time you add to something every
frame, multiply by `dt`.",
            code: Some(("spinner", SPINNER_LUA)),
            check: Check::Script("spinner"),
        },
        Step {
            title: "Attach it to the cube",
            body: "\
A script that isn't attached to anything does nothing at all — this is the step
people miss.

Select `Spinner` in the Hierarchy. In the **Inspector** (on the right), press
**➕ Add Component** and pick `spinner` from the **Scripts** group.

Or: drag `spinner.lua` from the **Assets** panel straight onto the node's row in
the Hierarchy. Same result.",
            code: None,
            check: Check::NodeRuns { node: "Spinner", script: "spinner" },
        },
        Step {
            title: "Press Play",
            body: "\
Hit **⏵ Play** in the toolbar (or press **F1**).

The cube turns. Press Play again to stop, and note that it snaps back to where
it started — play mode never changes your scene, so you can experiment with
absolutely no risk.

## If nothing happens

- Check the **Console** panel. A script with a mistake in it reports the file
  and the line, and keeps the rest of the game running.
- Check the script is actually attached (previous step), and that its checkbox
  in the Inspector is ticked.",
            code: None,
            check: Check::Played,
        },
        Step {
            title: "Make the speed adjustable",
            body: "\
Right now ninety degrees is baked into the code. Changing it means editing,
saving, and playing again — a slow loop for something you want to *feel* your
way to.

Add a `defaults` table and the Inspector builds a row for every value in it.
Open `scripts/spinner.lua` and replace what's there with the version below.

Now select `Spinner`, press Play, and **drag the `speed` slider while the game is
running**. The cube responds immediately.

## What just happened

- `defaults` declares the script's tunables. Anything you put in there shows up
  in the Inspector, in the order you wrote it.
- `params.speed` reads the current value — the one in the Inspector, not the
  one in the file. The default is only the starting point.
- The `--@` comments describe the row: `--@range` bounds it, `--@units` puts a
  suffix on the number, `--@desc` becomes its tooltip. They're comments, so
  nothing breaks if you delete them or spell one wrong.

This is the single most useful habit in the whole engine. A number you can drag
while the game runs is worth ten you have to guess at.",
            code: Some(("spinner", SPINNER_2_LUA)),
            check: Check::Contains {
                script: "spinner",
                needle: "params.speed",
                what: "reads params.speed",
            },
        },
        Step {
            title: "Drive it with the keyboard",
            body: "\
Add movement. Replace `spinner.lua` with the version below and press Play —
**W A S D**, or a gamepad stick, now pushes the cube around.

## Ask for actions, not for keys

    local x, y = input.axis2(\"Move\")

`Move` is a **named action**, defined in **⚙ Settings → Input** and already
bound to W A S D *and* the left stick. Asking for `Move` rather than for the W
key buys you three things without any extra work: gamepads, players who want to
rebind their controls, and code that still reads sensibly in a year.

`x` is -1 to 1 left-to-right, `y` is -1 to 1 back-to-forward. On a stick they're
smoothly in between.

## Why forward is minus Z

    node.z = node.z - y * params.nudge * dt

In Floptle, +X is right, +Y is up, and forward is **-Z**. So pushing the stick
forward should *decrease* z, which is where that minus comes from. It catches
everyone once.",
            code: Some(("spinner", SPINNER_3_LUA)),
            check: Check::Contains {
                script: "spinner",
                needle: "input.axis2",
                what: "asks the input map which way you're pushing",
            },
        },
        Step {
            title: "Where to go next",
            body: "\
You've now done the four things the rest of it is made of: made a node, given it
a behaviour, exposed a number, and read the player's input.

## Try breaking it, on purpose

- Take the `* dt` out and watch the speed become a property of your monitor.
- Set `speed` to 0 and drive around. Set it to 720.
- Change `node.yaw` to `node.y` and see what \"turning\" becomes.
- Attach `spinner` to the ball as well. One script, two nodes, separate
  Inspector values on each.

## Then pick a game

**Build Flappy** is the shortest — one button, one rule, and a finished game at
the end. **Build a 3D platformer** is the natural next one if you'd rather run
and jump around. Both assume only what you just did.

The **⚙ API** page of the Scripting tab lists every call the engine has, with an
example for most; the search there is the fastest way to answer \"what was that
called?\".",
            code: None,
            check: Check::Read,
        },
    ],
};

// ---------------------------------------------------------------------------
// 2. Platformer
// ---------------------------------------------------------------------------

const PLATFORMER: Tutorial = Tutorial {
    id: "platformer",
    title: "Build a 3D platformer",
    tagline: "Run, jump, ride a moving platform, collect coins, reach the goal.",
    level: Level::Intermediate,
    minutes: 45,
    template: Some("platformer"),
    intro: "\
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
",
    steps: &[
        Step {
            title: "Make the ground",
            body: "\
Start from a new empty project, or clear the starter scene — either is fine.

Add a cube (Hierarchy → **✚ New** → **■ Cube**), rename it `Ground`, and in
the Inspector set its **scale** to about `40, 1, 40`. That's a wide, thin slab.

Now give it a body: in the Inspector, **➕ Add Component → Rigidbody**, and set
its **mode** to **Static**. Static means \"solid, and it never moves\" — the
right answer for level geometry. The physics engine can skip it entirely except
when something touches it, which is why a level made of static bodies costs
almost nothing.

Drop a couple more cubes around as platforms while you're here. Same treatment:
scale them flat, Rigidbody, mode Static.",
            code: None,
            check: Check::Node("Ground"),
        },
        Step {
            title: "Make the player",
            body: "\
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

Press Play. It should fall and land. That's all you want from it right now.",
            code: None,
            check: Check::Node("Player"),
        },
        Step {
            title: "Movement and jumping",
            body: "\
Create the script below and attach it to `Player`.

## Why fixedUpdate and not update

`update` runs once per drawn frame, so it runs more often on a fast machine.
`fixedUpdate` runs exactly sixty times a second no matter what, and it is the
same clock physics steps on. Put anything that decides where things go in
`fixedUpdate` and your jump is the same height for everyone — put it in `update`
and it quietly isn't.

Rule of thumb: **gameplay in `fixedUpdate`, cameras in `lateUpdate`, everything
cosmetic in `update`.**

## Why we keep the vertical speed

    local vy = node.vy
    ...
    node.vel = vec3(x * params.speed, vy, -y * params.speed)

Gravity is the physics engine's job. If we wrote a `0` in there instead of `vy`,
we'd be overwriting gravity's work sixty times a second and the character would
float. So: read what physics decided vertically, keep it, replace the two
horizontal axes with what the player asked for.

## The forgiving ground check

`node.grounded` is true when the body is genuinely resting on something. It also
flickers off for a frame or two when you run down a slope — and a jump that
doesn't fire because of a flicker feels broken in a way players notice and can't
explain. So we also probe with a short `raycast` straight down and accept either
answer.",
            code: Some(("platformerPlayer", PLATFORMER_PLAYER_LUA)),
            check: Check::NodeRuns { node: "Player", script: "platformerPlayer" },
        },
        Step {
            title: "Press Play and walk around",
            body: "\
**W A S D** to move, **Space** to jump.

It works, and it's unpleasant — the camera is still the free-fly one from the
starter scene, so you're steering a character you have to chase manually. That's
the next step.

## Tune it while it runs

With `Player` selected and the game playing, drag `speed` and `jump` in the
Inspector. Find values you like before you write another line. Being able to do
this is most of why `defaults` exists, and it is much faster than reasoning
about what 8.5 metres per second ought to feel like.",
            code: None,
            check: Check::Played,
        },
        Step {
            title: "A camera that follows",
            body: "\
Select the `Camera` node. Remove the `freelook` script it came with (the **…**
beside it → **🗑 Remove**), then create and attach the one below.

Now wire it up: with `Camera` selected, the Inspector shows a `target` row with a
node picker. Drag `Player` from the Hierarchy onto it.

## Why a noderef instead of find(\"Player\")

    defaults = { target = noderef() }

`find(\"Player\")` searches the scene by name every time you call it, and the day
you rename the node to `Hero` it silently returns nothing — no error, just a
camera that stopped working. A `noderef` is wired in the Inspector, survives
renames, and shows you at a glance what's connected to what.

## Why lateUpdate

`lateUpdate` runs after physics has finished moving everything for this frame.
A camera that reads the player's position in `update` gets *last* frame's
position — a lag of one frame that reads as jitter, and gets worse the faster
the player is moving.

## Why ease and not lerp

    node.pos = ease(node.pos, want, params.smoothing, dt)

The usual `pos = lerp(pos, want, 0.1)` moves a tenth of the remaining distance
*per frame*, so the camera is stiffer at 240 fps than at 60. `ease` takes `dt`
and covers a rate you specify per **second** — identical on every machine.",
            code: Some(("platformerCamera", PLATFORMER_CAMERA_LUA)),
            check: Check::NodeRuns { node: "Camera", script: "platformerCamera" },
        },
        Step {
            title: "A platform that moves — and carries you",
            body: "\
Add a **Cube**, name it `Platform`, scale it to something like `4, 0.5, 4`, and
put it out over the edge of the ground somewhere interesting.

Give it a **Rigidbody** with **mode** set to **Kinematic**, then attach the
script below.

## The three body modes, in one line each

- **Static** — solid, never moves. Level geometry.
- **Dynamic** — pushed by gravity and everything else. Players, crates, debris.
- **Kinematic** — you move it, physics doesn't, but it still pushes dynamic
  bodies out of the way. Moving platforms, lifts, doors.

Kinematic is what makes riding work. A static platform can't move; a dynamic one
would sag and get knocked around by the player standing on it. A kinematic one
goes exactly where the script puts it and carries what's on top.

Press Play and jump onto it.",
            code: Some(("platformMover", PLATFORM_MOVER_LUA)),
            check: Check::NodeRuns { node: "Platform", script: "platformMover" },
        },
        Step {
            title: "The manager: score, respawn, HUD",
            body: "\
Add an **Empty** node, name it `Game`, and attach the script below.

## Why a manager

The score doesn't belong to any coin, and \"you fell off the world\" isn't the
player's business either. Both are facts about the *game*. Putting them on one
node means there is exactly one place to look when the score is wrong, and coins
never need to know how many other coins there are.

Other scripts reach it like this:

    local game = findScript(\"platformerGame\")
    if game then game.collect() end

`findScript` returns a handle to the first script of that kind anywhere in the
scene. Reading `game.collect` gets the function; note the **dot**, not a colon —
these are plain functions, not methods, so there's no `self` to pass.

## Why the functions aren't local

    function collect()

A `local function` is private to its file. These have to be reachable from
outside, so they're declared without `local`. That's the convention for anything
a manager publishes.

## The HUD is one call

`draw.text` puts a string on the screen in pixels, with no UI tree to build.
It's immediate mode: it draws for one frame, so it lives in `update` and is
re-issued every frame you want it visible. For a score counter that's exactly
right. (When you want buttons and layout, that's what the **◫ UI** tab is for.)",
            code: Some(("platformerGame", PLATFORMER_GAME_LUA)),
            check: Check::NodeRuns { node: "Game", script: "platformerGame" },
        },
        Step {
            title: "Tag the player",
            body: "\
Select `Player`. In the Inspector, find the **tags** row and add the tag
`player`.

## What tags are for

The coin is about to ask \"is the thing that touched me the player?\". It could
compare names — `other.name == \"Player\"` — but that breaks the moment you add a
second player, or rename the node, or spawn one from a prefab with a suffix.

A tag is a label you can put on any number of nodes and test cheaply:
`other:hasTag(\"player\")`. Group membership, not identity. Use tags for
\"what kind of thing is this\" and names for \"which one is it\".",
            code: None,
            check: Check::Tagged { node: "Player", tag: "player" },
        },
        Step {
            title: "Coins",
            body: "\
Add a **Sphere**, name it `Coin`, shrink it (**scale** around `0.4`), and float it
somewhere the player has to jump for.

Give it a **Rigidbody**, set its **mode** to **Kinematic**, and tick
**trigger**.

## What a trigger is

A trigger has a shape and reports what enters it, but doesn't block anything —
you walk straight through. That is exactly what a pickup is. The same switch
turns a wall into a doorway you get told about, which is how checkpoints,
damage zones and level exits all work.

Attach `coin` and press Play. Walk into it: it vanishes and the counter goes up.

Once one works, select it, **Ctrl+D** to duplicate, and scatter a dozen. Every
copy shares the one script.",
            code: Some(("coin", COIN_LUA)),
            check: Check::Script("coin"),
        },
        Step {
            title: "The goal",
            body: "\
Add one more node — a **Cube** works — name it `Goal`, put it at the end of your
level, give it a **Rigidbody** with **mode** **Kinematic** and **trigger** ticked, and
attach the script below.

Notice how little there is to it. It's `coin.lua` with one word changed. Once
you have a manager and triggers, most of \"the game part\" of a game is a trigger
that calls one function.

Press Play and finish your level.",
            code: Some(("goal", GOAL_LUA)),
            check: Check::Node("Goal"),
        },
        Step {
            title: "Make it yours",
            body: "\
You have a platformer. Now make it a *level* — the fastest way to learn what any
of these numbers do is to build something you actually want to get to the end
of.

## Things worth trying next, roughly in order of effort

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

## The finished version

Everything above is in the `platformer` starter template — create a project with
it from the Hub to compare notes.",
            code: None,
            check: Check::Read,
        },
    ],
};

// ---------------------------------------------------------------------------
// 3. Top-down RPG
// ---------------------------------------------------------------------------

const TOPDOWN: Tutorial = Tutorial {
    id: "topdown",
    title: "Build a top-down RPG",
    tagline: "Walk a village, talk to someone, pick up a key, unlock a door to another scene.",
    level: Level::Intermediate,
    minutes: 50,
    template: Some("topdown"),
    intro: "\
The bones of an adventure game: a character seen from above, someone with
something to say, an item worth having, and a door that wants it.

The new ideas here are the ones that make a game feel like a world rather than a
single screen — **state that outlives a scene**, and one script owning it.

## What you should already know

How to add a node, attach a script, and wire an Inspector reference. The
platformer tutorial covers all three, but you don't need to have finished it.

## The plan

1. A room, a character, and a camera that looks down at it.
2. An NPC you can stand next to and talk to.
3. An inventory — one manager, asked by everyone else.
4. A locked door, and a second scene behind it.
",
    steps: &[
        Step {
            title: "A room to walk around",
            body: "\
Add a **Cube**, name it `Ground`, set its **scale** to about `30, 1, 30`, and give it a
**Rigidbody** with **mode** **Static**.

Then wall it in: four more cubes, stood on end around the edge, each with a
Static Rigidbody. They don't need to be pretty — you're going to walk into them
in a minute to check they stop you.

## Do this with the Map tool instead, if you like

The **▦ Model** tab draws blockout geometry properly: rooms, corridors, steps,
with collision that matches what you see. For a real project it's the right
tool. Cubes are fine for today.",
            code: None,
            check: Check::Node("Ground"),
        },
        Step {
            title: "The character",
            body: "\
Add a **Capsule**, name it `Player`, and give it a **Rigidbody** left on
**Dynamic**, with **shape** Capsule and **freeze rot** on for x, y and z. The
same setup as any character.

Add the tag `player` on the **tags** row while you're here — the pickups and the door will look for
it.

Then create and attach the script below.

## What makes this top-down rather than third-person

    local move = vec3(x, 0, -y)

The input is used **as world directions**, with no camera in the maths at all.
In a third-person game, \"forward\" means \"away from the camera\", so the camera's
yaw has to be folded in. A top-down camera never turns, so up on the stick is
north and stays north — much simpler, and the correct behaviour for this genre.

## The diagonal problem

    if move:length() > 1 then move = move:normalized() end

Holding W and D gives you (1, 0, -1), which is about 1.41 long — so players who
walk diagonally walk 41% faster. Normalising caps the length at 1 while leaving
a gently-pushed analogue stick alone.",
            code: Some(("topdownPlayer", TOPDOWN_PLAYER_LUA)),
            check: Check::NodeRuns { node: "Player", script: "topdownPlayer" },
        },
        Step {
            title: "Look down at it",
            body: "\
Select the `Camera`, remove its `freelook` script (the **…** beside it →
**🗑 Remove**), attach the script below, and drag `Player` onto its `target`
row.

Press Play and walk into a wall to check your room actually contains you.

## The lead

`leadZ` puts the camera slightly *behind* where it's looking, so the player sits
a little below the middle of the screen and you can see more of what's ahead.
Set it to 0 for a strict overhead view and feel the difference — it's a small
number with a surprising effect on how far ahead you can plan.

Try `height` between about 10 and 25. Higher is more strategic and less personal;
this one number does more for the feel of a top-down game than anything else you
can change.",
            code: Some(("topdownCamera", TOPDOWN_CAMERA_LUA)),
            check: Check::NodeRuns { node: "Camera", script: "topdownCamera" },
        },
        Step {
            title: "Someone to talk to",
            body: "\
Add a **Capsule**, name it `Villager`, colour it differently, and attach the
script below. No Rigidbody needed — this one is measured by distance, not by
collision.

Press Play, walk up to them, and press **E**.

## One script, many villagers

    lines = \"Hello, traveller.|The cave is not safe.|Take this.\"

The dialogue is a **string parameter**, so it's edited in the Inspector, not in
the code. Duplicate the villager and the copy can say something completely
different while sharing the same script. This is the pattern to reach for
whenever you catch yourself about to copy a script and change one line in it.

`--@multiline` turns the Inspector row into a proper text box.

## Distance, not a trigger

    if distance(node, player) < params.range then

For \"am I near enough to interact\" a distance test is simpler than a trigger
volume, needs no collider, and is trivially tunable. `distance` takes node
handles directly, so there's no vector arithmetic to get wrong.",
            code: Some(("npcTalk", NPC_TALK_LUA)),
            check: Check::NodeRuns { node: "Villager", script: "npcTalk" },
        },
        Step {
            title: "An inventory that survives the scene",
            body: "\
Add an **Empty** node called `Game` and attach the script below.

## The bit that matters: save

    items = save.get(\"items\", {})
    ...
    save.set(\"items\", items)

When you walk through a door in the next step, `scene.load` replaces the entire
world — every node is destroyed, the new scene's nodes are created, and every
`start()` runs again. Anything held in a plain variable is gone.

`save.*` is a store that outlives all of that: scene loads, stopping and
starting the game, quitting the editor, and the exported build. Write the small
fact — what you're carrying, which doors are open, how much gold — not the whole
world. There's a size limit per key (about a kilobyte) that enforces the habit.

## Ask the owner, don't keep copies

Nothing else stores what you're carrying. The pickup calls `add`, the door calls
`has`, and if the answer is ever wrong there is exactly one file to open.",
            code: Some(("inventory", INVENTORY_LUA)),
            check: Check::NodeRuns { node: "Game", script: "inventory" },
        },
        Step {
            title: "Something to pick up",
            body: "\
Add a **Cube**, name it `Key`, shrink it, and put it near the villager. Give it
a **Rigidbody** with **mode** **Kinematic** and **trigger** ticked, then attach
the script below.

Set its `item` row in the Inspector to `Rusty Key` — the door is going to ask
for that exact string, so watch the spelling.

Press Play and walk over it. The Console prints what you picked up, and the
counter in the corner goes up.",
            code: Some(("itemPickup", ITEM_PICKUP_LUA)),
            check: Check::NodeRuns { node: "Key", script: "itemPickup" },
        },
        Step {
            title: "A second scene",
            body: "\
Right-click an empty part of the **Assets** panel and choose **⎙ New Scene**.
Name it `cave`. It's created as `scenes/cave.ron` and opened straight away.

Build something small in it — a floor, some walls, a light. Crucially, give it
its own `Player` and `Camera` set up exactly like the first scene's, because
`scene.load` throws everything away and builds the new scene from its own file.

Save it (**Ctrl+S**), then **double-click `scenes/first.ron`** in the Assets
panel to go back.

## Two ways to carry things across

- **save.\\*** — the small facts. What you're carrying, which bosses are dead.
  This is what the inventory already does, and why it will still know about your
  key on the other side.
- **`node.persistent`** — mark a node and it survives the swap intact. Useful
  for a music player or a persistent HUD; overkill for data.",
            code: None,
            check: Check::Scene("cave"),
        },
        Step {
            title: "The locked door",
            body: "\
Back in the first scene, add a **Cube**, name it `Door`, stand it in a wall,
give it a **Rigidbody** with **mode** **Kinematic** and **trigger** ticked, and
attach the script below.

Check its Inspector rows: `destination` should be `cave`, and `needs` should
be `Rusty Key` — spelled exactly as the pickup spells it.

Now play it properly: walk into the door *first* and get refused, then go and
get the key, then come back. That loop — a thing that wants a thing — is most
of adventure-game design.

## Reading the refusal

    after(2.5, function() blocked = false end)

`after` runs something once, later, on the game clock. No timer variable to
count down in `update`, and it pauses when the game does. `every` and `tween`
are its siblings; all three hand back a handle with `:cancel()` on it.",
            code: Some(("door", DOOR_LUA)),
            check: Check::NodeRuns { node: "Door", script: "door" },
        },
        Step {
            title: "Where an RPG goes from here",
            body: "\
You have the loop: explore, talk, acquire, unlock. Everything else an RPG does
is a variation on parts you've now built.

## The next pieces, and which one you already know

- **More items** — duplicate the Key, change the Item string. Done already.
- **A shop** — an NPC whose `Interact` calls into `inventory` instead of
  printing a line. Same two scripts.
- **Combat** — a trigger on a swing, `findTagged(\"enemy\")` to see who's in
  range, a `health` script per enemy holding one number.
- **Quests** — a manager beside `inventory` holding \"which step of which quest\",
  written to `save.*` the same way.
- **Real dialogue** — when `draw.text` runs out of road, the **◫ UI** tab
  builds proper panels with layout, and `ui.on(\"clicked\", ...)` handles the
  buttons.

## A word on structure, now you have three managers' worth of experience

The scripts that stayed small are the ones that only know about their own node.
The scripts that grow are the managers. When a manager gets uncomfortable, split
it by *what it owns* — `inventory`, `quests`, `dialogue` — rather than by what
it does. Ownership is the thing that keeps two scripts from disagreeing.

The finished version of all of this is the `topdown` starter template.",
            code: None,
            check: Check::Read,
        },
    ],
};

// ---------------------------------------------------------------------------
// 4. Flappy
// ---------------------------------------------------------------------------

const FLAPPY: Tutorial = Tutorial {
    id: "flappy",
    title: "Build Flappy",
    tagline: "One button, endless obstacles, a score, and a game over you can restart.",
    level: Level::Beginner,
    minutes: 30,
    template: Some("flappy"),
    intro: "\
The shortest complete game there is: one button, one rule, and a number that
goes up. You can finish this in half an hour and you'll have made something with
a beginning, a middle and an end — which is more than most first projects
manage.

It also teaches the two things every arcade game needs and the other tutorials
don't: **spawning objects while the game runs**, and **a game state that can
end and start again**.

## A 3D engine playing a 2D game

Floptle is 3D. Flappy is flat. The trick is simply to refuse to use the third
axis: everything sits at z = 0, and the camera looks straight down it. There's
no 2D mode to switch on, and you don't need one.
",
    steps: &[
        Step {
            title: "Point the camera down the Z axis",
            body: "\
Start from a fresh project. Delete the starter crate, ball and capsule — you
want an empty stage.

Select the `Camera` and remove its `freelook` script (the **…** beside it →
**🗑 Remove**) — otherwise you will fly the camera off by accident mid-game.

Set the camera's **position** to about `0, 4, 18` and its **rotation** to all
zeroes. It now looks along -Z at a flat plane, which is the whole stage.

Everything you place from here goes at **z = 0**.",
            code: None,
            check: Check::Read,
        },
        Step {
            title: "The bird",
            body: "\
Add a **Sphere**, name it `Bird`, put it at about `-4, 4, 0`, and shrink it a
little.

Give it a **Rigidbody**, left on **Dynamic**, with **shape** Sphere and
**affected by gravity** ticked. Press Play: it falls off the bottom of the
screen. That is the game working correctly — falling is the default state, and
the entire player input is a refusal to.

## Then create and attach the script

Press Play again. **Space** now flaps.

## Why we set velocity rather than add to it

    node.vel = vec3(0, params.flap, 0)

Adding force would make each flap depend on how fast you were already falling —
tap twice quickly and you'd rocket away. *Replacing* the velocity means every
flap is identical wherever you were, which is what makes the game learnable.
Nearly every arcade jump works this way.

## Staying on the plane

Nothing pushes the bird in Z, but a collision later might. The two lines that
snap `z` back to 0 cost nothing and save a confusing bug where the bird slowly
drifts behind the pipes and sails through them.",
            code: Some(("flappyBird", FLAPPY_BIRD_LUA)),
            check: Check::NodeRuns { node: "Bird", script: "flappyBird" },
        },
        Step {
            title: "Build one pipe and give it its behaviour",
            body: "\
Add an **Empty** node called `Pipe`. Under it — drag them onto its row in the
Hierarchy to parent them — add two **Cubes**: one stretched upward above the
gap, one below it.

The two cubes are what the bird actually hits, so give each a **Rigidbody** with
**mode** **Kinematic**. Kinematic, not Static, because the script below moves
the parent every frame and a static body is a baked collider that would stay
behind.

Then create the script and attach it to the `Pipe` **root** (not to the cubes —
moving the parent carries both halves).

## Every pipe cleans up after itself

    if node.x < -params.behind then
      node:destroy()
    end

The alternative is a spawner holding a list of every pipe it ever made,
remembering to walk it, and leaking the ones it forgets. Letting each object
decide when it is finished is smaller, and it stays correct when something
destroys a pipe for a reason the spawner never hears about.

## Scoring on the pipe, not on the bird

The pipe knows when it has passed x = 0. The bird would have to check every pipe
every frame to work the same thing out. Put the decision where the information
already is — that principle will save you more code than any other one here.

The `scored` flag is what stops one pipe scoring sixty times a second while it
crosses the line.",
            code: Some(("flappyPipe", FLAPPY_PIPE_LUA)),
            check: Check::Script("flappyPipe"),
        },
        Step {
            title: "Save it as a prefab",
            body: "\
Drag the `Pipe` node from the Hierarchy into the **Assets** panel. That writes
`prefabs/Pipe.prefab.ron` — the node, its two children, their bodies and the
script, all saved together.

Then **delete** the `Pipe` node from the scene. The spawner will make its own.

## What a prefab is, and why this game needs one

A prefab is a saved node and everything under it. `spawn(\"Pipe\", position)`
stamps out a fresh copy while the game runs.

You need one here because a script can *create* nodes but can't give them
colliders. Anything that has to be solid is authored once and spawned. That is
not a limitation you'll fight — it is the normal way to make bullets, enemies,
debris and pipes.

Changing a prefab later works the same way round: drag it back into the scene,
edit it, drag it into Assets again.",
            code: None,
            check: Check::Prefab("Pipe"),
        },
        Step {
            title: "The rules",
            body: "\
Add an **Empty** node called `Game` and attach the script below.

Press Play. Pipes arrive, the score counts, hitting one ends it, and Space
starts again.

## Spawning on a schedule

    spawner = every(params.interval, function()
      ...
    end)

`every` repeats on the game clock and hands back a handle you can `:cancel()`.
Doing this with a countdown in `update` is four more lines and one more thing to
get wrong; more importantly, `every` pauses when the game does.

Cancelling it in `lose()` is what stops pipes marching on over the game-over
screen.

## A published variable

    over = false

Declared with no `local`, on purpose. The bird and the pipes read
`game.over` through their script handles. Locals are private to a file; this one
is the script's public state, and that difference is worth being deliberate
about.

## Restarting

    scene.load(scene.current())

Reloading the current scene is a complete reset: every node back to its authored
state, every script's `start()` run again. It's the cheapest possible restart
and it is exactly right for an arcade game.

Note the high score goes through `save.*`, so it survives the reload — and
quitting, and the exported build.",
            code: Some(("flappyGame", FLAPPY_GAME_LUA)),
            check: Check::NodeRuns { node: "Game", script: "flappyGame" },
        },
        Step {
            title: "Tune it until it's actually fun",
            body: "\
This is the real work, and it is entirely done from the Inspector with the game
running. Nothing below needs a code change.

- `flap` on the bird, and `interval`, `gapLow` and `gapHigh` on the game, are
  the numbers that decide whether this is playable. Move one at a time.
- If it's too hard, widen the gap before you slow the pipes down — a slow game
  is boring in a way a hard one isn't.
- Make the gap narrow as the score climbs. Two lines in `score()`.

Press Play and get a score you're pleased with before moving on. Playing your
own game for five minutes will tell you more than reading about game feel for an
hour.",
            code: None,
            check: Check::Played,
        },
        Step {
            title: "Make it a real game",
            body: "\
It's finished. Everything from here is polish — which is most of what separates
a project from a game.

## Cheap wins, roughly in order

- **Sound.** A flap, a score blip, a crash. The **≣ Mixer** tab, and
  `node:sound()`.
- **A particle burst on death** — the **✱ Particles** tab, then
  `spawnEffect(\"Crash\", node.pos)`.
- **A start screen.** `over = true` at the beginning and a different message
  until the first Space.
- **Something behind it.** A few slow-moving background shapes at negative z
  give an enormous amount of depth for almost nothing.
- **Ship it.** **File → Export Game** builds a standalone executable for
  Windows, macOS or Linux. This is a small enough game to actually finish and
  hand to someone, which is a rare and worthwhile thing.

## And then

The **3D platformer** tutorial covers cameras, moving platforms and level
structure; the **top-down RPG** covers state that outlives a scene. Between the
three you have seen most of the engine's shape.

The finished version of this one is the `flappy` starter template.",
            code: None,
            check: Check::Read,
        },
    ],
};

// ---------------------------------------------------------------------------
// 5. For programmers
// ---------------------------------------------------------------------------

const FOR_PROGRAMMERS: Tutorial = Tutorial {
    id: "for-programmers",
    title: "Floptle for programmers",
    tagline: "The model, the tick, and the six things that aren't like the engine you came from.",
    level: Level::Programmer,
    minutes: 20,
    template: None,
    intro: "\
You've shipped software. You don't need `dt` explained. What you need is the
model — what a node actually is, what runs when, where state is supposed to
live, and which of your habits from the last engine will quietly cost you an
afternoon here.

No project to build. Read it front to back in about twenty minutes, then use the
**⚙ API** page for specifics; it lists every name the engine exposes.

The last step has something to type, because a model you haven't run is a model
you're guessing at.
",
    steps: &[
        Step {
            title: "What's on disk",
            body: "\
A project is a directory of plain text. No database, no binary blob, no import
step you can't see.

    project.ron        settings: title, entry scene, layers, mixer graph
    scenes/*.ron       node trees
    scripts/*.lua      behaviour
    prefabs/*.prefab.ron  saved subtrees
    materials/  models/  textures/  audio/  vfx/  ui/
    .floptle/          editor-side extras (Lua definitions, tutorial progress)

Everything is RON — Rust's object notation, which reads like JSON that grew up.
It diffs and merges as well as text can, so it belongs in version control
exactly as it is.

Two consequences worth internalising:

- **The editor is not the source of truth; the files are.** External edits are
  picked up, and anything the editor can do to a project, `floptle --new`,
  `--migrate` and `--export` can do headlessly from CI.
- **A `.glb` you drop in is referenced, not absorbed.** Re-export it over the
  same path and the scene picks it up.",
            code: None,
            check: Check::Read,
        },
        Step {
            title: "Nodes, components, and what a script attaches to",
            body: "\
Underneath there's an ECS: entities with component columns. Above it there's a
**node tree**, which is what the editor and the scripts talk to. Think of the
node as a facade — it's the thing with a name, a transform, a parent, and a set
of components.

    Node = Name + Transform + Parent? + Matter + [components…]

`Matter` is the mutually-exclusive one — a node is a Primitive **or** a Mesh
**or** a Camera **or** a Light **or** a UI element, never two. Everything else
is additive: Rigidbody, Material, Scripts, Tags, Layer, Networked.

A **script** is a component holding `(kind, enabled, params)`. `kind` is the
file stem, so `scripts/patrol.lua` attaches as `patrol`. The same file attached
to forty nodes is forty independent instances: each gets its own environment,
its own file-scope locals, its own `params`.

That last point is the one to hold on to. A file-scope `local` in a Lua script is
**per-instance state**, not per-file. It behaves the way a struct field would in
a language with objects, and it is why almost none of these scripts define a
class.",
            code: None,
            check: Check::Read,
        },
        Step {
            title: "The tick, and which hook to use",
            body: "\
Three per-node hooks, and choosing between them correctly is most of the
frame-timing bugs you'll never have:

- **`fixedUpdate(node, dt)`** — 60 Hz, constant `dt`, the same clock physics
  steps on. Movement, gameplay, anything that decides where things go. Frame
  rate cannot change the outcome.
- **`update(node, dt)`** — once per drawn frame, variable `dt`. Cosmetics, HUD,
  input polling that isn't authoritative.
- **`lateUpdate(node, dt)`** — after physics **and** after the interpolated
  transform writeback. The camera pass. Anything that follows something else
  belongs here; do it in `update` and you are following last frame's pose,
  which is a `velocity × dt` lag that reads as jitter.

Plus `start(node)` once at play, and the event hooks: `onCollisionEnter/Stay/Exit`,
`onTriggerEnter/Stay/Exit`, and the UI hooks (`clicked`, `changed`, `dragStart`,
…). All take the node first — always, including `start`.

Alongside that there's a scheduler on the game clock: `after(s, fn)`,
`every(s, fn)`, `tween(s, fn, ease)`, each returning a handle with `:cancel()`.
They pause when the game pauses, which hand-rolled countdowns in `update` do
not.",
            code: None,
            check: Check::Read,
        },
        Step {
            title: "Params: the Inspector as a two-way binding",
            body: "\
    defaults = {
      --@range 0 20 --@units m/s
      speed = 4.5,
      --@options patrol|chase|flee
      mode = \"patrol\",
      target = noderef(),
      body = componentref(\"RigidBody\"),
      brain = scriptref(\"health\"),
    }

`defaults` declares the instance's tunables; the Inspector generates a row per
entry, in declaration order, with the widget the value's *type* implies — number,
string, bool, colour, dropdown. `--@` annotations describe the row and are inert
at runtime.

Three things that aren't obvious:

- **It is two-way.** Writing `params.speed` from Lua persists across frames,
  shows live in the Inspector during play, and is readable by other scripts
  through a handle. Stop reverts it to the authored value.
- **The ref types eliminate `find()`.** `noderef` gives you a node handle
  wired in the editor; `componentref` and `scriptref` bind straight to a
  component or another script on that node. Renaming a node doesn't break any
  of them, and you can see the wiring in the Inspector rather than inferring it
  from string literals.
- **Undeclared keys are frame-local.** If it isn't in `defaults`, it doesn't
  round-trip.",
            code: None,
            check: Check::Read,
        },
        Step {
            title: "Talking between scripts",
            body: "\
In rough order of how much you should prefer them:

- **`scriptref` / `noderef` params** — wired in the editor. No search, no string
  literal, survives renames.
- **`findScript(\"kind\")`** — a handle to the first script of that kind
  anywhere. The manager pattern: one `inventory`, one `gameState`, asked by
  everyone. `findScripts` gets all of them.
- **`find(\"Name\")` / `findTagged(\"tag\")`** — by name, or by group. Cache the
  result in `start`; calling `find` every frame is a scene walk you're paying
  for repeatedly.

A **script handle** proxies the target's environment: `h.someState` reads its
variable, `h.someFn()` calls its function, `h.node` is the node it's on, and
`h.valid` says whether it still exists. Note the dot — these are plain
functions, so a colon would pass the handle as a phantom first argument.

The convention for publishing state is a file-scope assignment with **no**
`local`. Locals are private to the file; a bare `over = false` is deliberately
the script's public surface. The linter knows the difference and won't flag it.",
            code: None,
            check: Check::Read,
        },
        Step {
            title: "Lua's one real hazard, and the lints",
            body: "\
Lua has a defining trap: **every undeclared name is a global that reads `nil`.**

    local speed = 4
    sped = speed * dt   -- compiles, runs, does nothing, says nothing

Nothing raises. Combine it with hot reload and you can lose an afternoon to a
script that \"should work\". The editor lints for exactly this, plus:

- **unused local** — usually a half-finished rename.
- **upvalue pressure** — LuaJIT allows 60 upvalues per function, and every
  file-scope `local` is an upvalue of every function below it. The real error
  (\"too many upvalues\") names no fix, so this warns at 50 with one.
- **hook signature** — `function update(dt)` binds the *node* to `dt`. From the
  outside that's a script that does nothing at all.
- **raw key polls** — `input.pressed(\"space\")` where a named action would work.
  It runs; it just can't be rebound, never reaches a gamepad, and reads neutral
  on a networked predicted node.

All warnings, never blocking, `--@nolint` to silence a line or a file. The
runtime is **LuaJIT** (5.1 semantics plus `goto`): `math.atan2` and `#t`, no
integer division operator, no `goto continue` idiom needed.",
            code: None,
            check: Check::Read,
        },
        Step {
            title: "Six things that will surprise you",
            body: "\
The list of habits that don't transfer, collected from the projects built in
this engine:

- **Forward is -Z.** +X right, +Y up. Every direction bug traces back here once.
- **Scripts can create nodes but not colliders.** `createNode` and
  `n:setPrimitive(...)` build geometry at runtime; anything that must be solid
  is authored once as a prefab and `spawn`ed. Procedural *levels* are prefabs
  placed by script, not geometry conjured by it.
- **A parent link in a scene file is positional.** Inserting a node in the
  middle of a `.ron` by hand shifts every later index. Let the editor do it.
- **`save.*` is capped at about a kilobyte per key.** Deliberately. Store the
  fact, not the world.
- **Play mode never mutates your scene.** Everything reverts on Stop, params
  included. It's a sandbox — but it also means \"I fixed it while playing\" is a
  thing you have to redo.
- **Named actions, always.** `input.action(\"Fire\")` over a key code. Gamepads,
  rebinding, and multiplayer prediction all fall out of it for free; raw key
  polls read neutral on a predicted node, which is a bug that only appears once
  a second player joins.",
            code: None,
            check: Check::Read,
        },
        Step {
            title: "Determinism, if you're going near multiplayer",
            body: "\
Skip this until it matters, but know it exists — retrofitting it is much worse
than allowing for it.

Networking is **rollback**: clients predict forward from local input, the server
is authoritative, and mispredictions re-simulate from the last agreed state.
Re-simulation means the same inputs must produce the same outputs, every time,
on every machine.

What that costs you in practice:

- Gameplay in `fixedUpdate`. Anything driven by frame time cannot re-simulate.
- `rng(seed)` for anything that matters — a deterministic stream. `math.random`
  is fine for a spark, not for loot.
- Read input through named actions. Raw key polls are not part of the input
  command that gets replayed, so they read neutral during prediction.
- `net.isServer()` / `net.isMine(node)` to decide who's allowed to act, and
  `net.spawn` rather than `spawn` for anything replicated.

`docs/multiplayer.md` is the long version, and there's a referee mode that
replays a recorded match and reports the first frame where two machines
disagreed.",
            code: None,
            check: Check::Read,
        },
        Step {
            title: "Run one",
            body: "\
Enough reading. Create the script below, put it on any node, and press Play.

It's forty seconds of work and it makes four of the abstractions above concrete
at once: per-instance state, the difference between the two clocks, two-way
params, and the scheduler.

Watch the Console. `update` and `fixedUpdate` tick at different rates and the
gap between them is your frame rate; drag **Rate** in the Inspector while it
runs and the log responds immediately.

## Then

- **⚙ API** in the Scripting tab, or `docs/lua-api.md` — every name, grouped,
  searchable, with worked examples.
- The scripts every project is seeded with, in `scripts/` — `third_person`,
  `fighter`, the `rts_*` trio and `web_login` are reference implementations,
  and they are written to be read.
- `docs/scripting.md` for the long form, `docs/ARCHITECTURE.md` for what's
  under the node tree.",
            code: Some(("probe", PROBE_LUA)),
            check: Check::Script("probe"),
        },
    ],
};
