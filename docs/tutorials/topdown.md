# Build a top-down RPG

Walk a village, talk to someone, pick up a key, unlock a door to another scene.

**some coding** · about 50 minutes · 9 steps

The finished project is a starter template: create a new project with the **topdown** template (in the Hub, or `floptle --new <dir> --template topdown`) to read the answer.

> Follow this along **inside the editor** — the 🎓 Learn tab has the same steps and ticks each one off as your project starts to match it.

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

## 1. A room to walk around

Add a **Cube**, name it `Ground`, scale it to about `30, 1, 30`, and give it a
**Rigidbody** with **mode** **Static**.

Then wall it in: four more cubes, stood on end around the edge, each with a
Static Rigidbody. They don't need to be pretty — you're going to walk into them
in a minute to check they stop you.

### Do this with the Map tool instead, if you like

The **▦ Map** tab draws blockout geometry properly: rooms, corridors, steps,
with collision that matches what you see. For a real project it's the right
tool. Cubes are fine for today.

*Done when: a node called Ground is in the scene.*

## 2. The character

Add a **Capsule**, name it `Player`, and give it a **Rigidbody** left on
**Dynamic**, with **shape** Capsule and **freeze rot** on for x, y and z. The
same setup as any character.

Add the tag `player` while you're here — the pickups and the door will look for
it.

Then create and attach the script below.

### What makes this top-down rather than third-person

    local move = vec3(x, 0, -y)

The input is used **as world directions**, with no camera in the maths at all.
In a third-person game, "forward" means "away from the camera", so the camera's
yaw has to be folded in. A top-down camera never turns, so up on the stick is
north and stays north — much simpler, and the correct behaviour for this genre.

### The diagonal problem

    if move:length() > 1 then move = move:normalized() end

Holding W and D gives you (1, 0, -1), which is about 1.41 long — so players who
walk diagonally walk 41% faster. Normalising caps the length at 1 while leaving
a gently-pushed analogue stick alone.

`scripts/topdownPlayer.lua`

```lua
-- Eight-way movement on the ground plane, seen from above.

defaults = {
  --@range 0 20 --@units m/s
  speed = 5.0,
  --@desc Turn to face the way you are walking.
  faceTravel = true,
}

function fixedUpdate(node, dt)
  local x, y = input.axis2("Move")

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
end
```

*Done when: Player runs topdownPlayer.lua.*

## 3. Look down at it

Select the `Camera`, remove its `freelook` script (the **…** beside it →
**🗑 Remove**), attach the script below, and drag `Player` onto its **Target**
row.

Press Play and walk into a wall to check your room actually contains you.

### The lead

`leadZ` puts the camera slightly *behind* where it's looking, so the player sits
a little below the middle of the screen and you can see more of what's ahead.
Set it to 0 for a strict overhead view and feel the difference — it's a small
number with a surprising effect on how far ahead you can plan.

Try `height` between about 10 and 25. Higher is more strategic and less personal;
this one number does more for the feel of a top-down game than anything else you
can change.

`scripts/topdownCamera.lua`

```lua
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
end
```

*Done when: Camera runs topdownCamera.lua.*

## 4. Someone to talk to

Add a **Capsule**, name it `Villager`, colour it differently, and attach the
script below. No Rigidbody needed — this one is measured by distance, not by
collision.

Press Play, walk up to them, and press **E**.

### One script, many villagers

    lines = "Hello, traveller.|The cave is not safe.|Take this."

The dialogue is a **string parameter**, so it's edited in the Inspector, not in
the code. Duplicate the villager and the copy can say something completely
different while sharing the same script. This is the pattern to reach for
whenever you catch yourself about to copy a script and change one line in it.

`--@multiline` turns the Inspector row into a proper text box.

### Distance, not a trigger

    if distance(node, player) < params.range then

For "am I near enough to interact" a distance test is simpler than a trigger
volume, needs no collider, and is trivially tunable. `distance` takes node
handles directly, so there's no vector arithmetic to get wrong.

`scripts/npcTalk.lua`

```lua
-- Stand close, press Interact (E, or West on a pad), read the next line.
--
-- The lines are a string param split on "|", so one script covers every
-- villager in the game and each says something different.

defaults = {
  --@multiline
  --@desc Each line separated by a | character.
  lines = "Hello, traveller.|The cave to the north is not safe.|Here, take this key.",
  --@range 0 10 --@units m
  range = 2.5,
  --@desc Shown when you are close enough to talk.
  prompt = "E — talk",
}

local said = 0
local player
local dialogue = {}

function start(node)
  player = find("Player")
  dialogue = {}
  -- Split on the separator. gmatch walks every run of not-a-pipe.
  for line in string.gmatch(params.lines, "[^|]+") do
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

  if input.justPressed("Interact") then
    said = said + 1
    if said > #dialogue then said = 1 end
  end

  local w, h = camera.screenSize()
  if said == 0 then
    draw.text(w * 0.5, h - 90, params.prompt, 20, 1, 1, 1, 0.75, "center")
  else
    draw.text(w * 0.5, h - 90, dialogue[said], 24, 1, 1, 1, 1, "center")
  end
end
```

*Done when: Villager runs npcTalk.lua.*

## 5. An inventory that survives the scene

Add an **Empty** node called `Game` and attach the script below.

### The bit that matters: save

    items = save.get("items", {})
    ...
    save.set("items", items)

When you walk through a door in the next step, `scene.load` replaces the entire
world — every node is destroyed, the new scene's nodes are created, and every
`start()` runs again. Anything held in a plain variable is gone.

`save.*` is a store that outlives all of that: scene loads, stopping and
starting the game, quitting the editor, and the exported build. Write the small
fact — what you're carrying, which doors are open, how much gold — not the whole
world. There's a size limit per key (about a kilobyte) that enforces the habit.

### Ask the owner, don't keep copies

Nothing else stores what you're carrying. The pickup calls `add`, the door calls
`has`, and if the answer is ever wrong there is exactly one file to open.

`scripts/inventory.lua`

```lua
-- What the player is carrying, and the only script that knows it.
--
-- Pickups call add(); the door calls has(). One owner means one place to look
-- when the answer is wrong.

local items = {}

function start(node)
  -- save.* outlives scene loads, Stop, and quitting the editor — so what you
  -- picked up in the village is still yours inside the cave.
  items = save.get("items", {})
end

function add(item)
  items[#items + 1] = item
  save.set("items", items)
  log("picked up " .. item)
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
  draw.text(w - 24, 24, "Carrying: " .. #items, 20, 1, 1, 1, 0.8, "right")
end
```

*Done when: Game runs inventory.lua.*

## 6. Something to pick up

Add a **Cube**, name it `Key`, shrink it, and put it near the villager. Give it
a **Rigidbody** with **mode** **Kinematic** and **trigger** ticked, then attach
the script below.

Set its **Item** row in the Inspector to `Rusty Key` — the door is going to ask
for that exact string, so watch the spelling.

Press Play and walk over it. The Console prints what you picked up, and the
counter in the corner goes up.

`scripts/itemPickup.lua`

```lua
-- Picked up by walking over it. The item's NAME is a param, so one script and
-- one prefab cover every item in the game.

defaults = {
  item = "Rusty Key",
}

function onTriggerEnter(node, other, hit)
  if not other:hasTag("player") then return end

  local bag = findScript("inventory")
  if bag then bag.add(params.item) end

  node:destroy()
end
```

*Done when: Key runs itemPickup.lua.*

## 7. A second scene

Right-click an empty part of the **Assets** panel and choose **⎙ New Scene**.
Name it `cave`. It's created as `scenes/cave.ron` and opened straight away.

Build something small in it — a floor, some walls, a light. Crucially, give it
its own `Player` and `Camera` set up exactly like the first scene's, because
`scene.load` throws everything away and builds the new scene from its own file.

Save it (**Ctrl+S**), then **double-click `scenes/first.ron`** in the Assets
panel to go back.

### Two ways to carry things across

- **save.\*** — the small facts. What you're carrying, which bosses are dead.
  This is what the inventory already does, and why it will still know about your
  key on the other side.
- **`node.persistent`** — mark a node and it survives the swap intact. Useful
  for a music player or a persistent HUD; overkill for data.

*Done when: scenes/cave.ron exists.*

## 8. The locked door

Back in the first scene, add a **Cube**, name it `Door`, stand it in a wall,
give it a **Rigidbody** with **mode** **Kinematic** and **trigger** ticked, and
attach the script below.

Check its Inspector rows: **Destination** should be `cave`, and **Needs** should
be `Rusty Key` — spelled exactly as the pickup spells it.

Now play it properly: walk into the door *first* and get refused, then go and
get the key, then come back. That loop — a thing that wants a thing — is most
of adventure-game design.

### Reading the refusal

    after(2.5, function() blocked = false end)

`after` runs something once, later, on the game clock. No timer variable to
count down in `update`, and it pauses when the game does. `every` and `tween`
are its siblings; all three hand back a handle with `:cancel()` on it.

`scripts/door.lua`

```lua
-- A way into another scene, and a lock if you name a key.
--
-- scene.load swaps the whole world: every node goes, the new scene's arrive,
-- and every start() runs again. Anything that must survive lives in save.*
-- (which is why the inventory writes there).

defaults = {
  --@desc The scene to load — a file stem under scenes/.
  destination = "cave",
  --@desc Leave this empty for a door that is not locked.
  needs = "Rusty Key",
  --@multiline
  refusal = "It's locked. Something about a key.",
}

local blocked = false

function onTriggerEnter(node, other, hit)
  if not other:hasTag("player") then return end

  if params.needs ~= "" then
    local bag = findScript("inventory")
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
  draw.text(w * 0.5, h * 0.5, params.refusal, 24, 1, 0.7, 0.6, 1, "center")
end
```

*Done when: Door runs door.lua.*

## 9. Where an RPG goes from here

You have the loop: explore, talk, acquire, unlock. Everything else an RPG does
is a variation on parts you've now built.

### The next pieces, and which one you already know

- **More items** — duplicate the Key, change the Item string. Done already.
- **A shop** — an NPC whose `Interact` calls into `inventory` instead of
  printing a line. Same two scripts.
- **Combat** — a trigger on a swing, `findTagged("enemy")` to see who's in
  range, a `health` script per enemy holding one number.
- **Quests** — a manager beside `inventory` holding "which step of which quest",
  written to `save.*` the same way.
- **Real dialogue** — when `draw.text` runs out of road, the **◫ UI** tab
  builds proper panels with layout, and `ui.on("clicked", ...)` handles the
  buttons.

### A word on structure, now you have three managers' worth of experience

The scripts that stayed small are the ones that only know about their own node.
The scripts that grow are the managers. When a manager gets uncomfortable, split
it by *what it owns* — `inventory`, `quests`, `dialogue` — rather than by what
it does. Ownership is the thing that keeps two scripts from disagreeing.

The finished version of all of this is the `topdown` starter template.

