# Build Flappy

One button, endless obstacles, a score, and a game over you can restart.

**no experience needed** · about 30 minutes · 7 steps

The finished project is a starter template: create a new project with the **flappy** template (in the Hub, or `floptle --new <dir> --template flappy`) to read the answer.

> Follow this along **inside the editor** — the 🎓 Learn tab has the same steps and ticks each one off as your project starts to match it.

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

## 1. Point the camera down the Z axis

Start from a fresh project. Delete the starter crate, ball and capsule — you
want an empty stage.

Select the `Camera` and remove its `freelook` script (the **…** beside it →
**🗑 Remove**) — otherwise you will fly the camera off by accident mid-game.

Set the camera's **Position** to about `0, 4, 18` and its **Rotation** to all
zeroes. It now looks along -Z at a flat plane, which is the whole stage.

Everything you place from here goes at **z = 0**.

## 2. The bird

Add a **Sphere**, name it `Bird`, put it at about `-4, 4, 0`, and shrink it a
little.

Give it a **Rigidbody**, left on **Dynamic**, with **shape** Sphere and
**affected by gravity** ticked. Press Play: it falls off the bottom of the
screen. That is the game working correctly — falling is the default state, and
the entire player input is a refusal to.

### Then create and attach the script

Press Play again. **Space** now flaps.

### Why we set velocity rather than add to it

    node.vel = vec3(0, params.flap, 0)

Adding force would make each flap depend on how fast you were already falling —
tap twice quickly and you'd rocket away. *Replacing* the velocity means every
flap is identical wherever you were, which is what makes the game learnable.
Nearly every arcade jump works this way.

### Staying on the plane

Nothing pushes the bird in Z, but a collision later might. The two lines that
snap `z` back to 0 cost nothing and save a confusing bug where the bird slowly
drifts behind the pipes and sails through them.

`scripts/flappyBird.lua`

```lua
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
  local game = findScript("flappyGame")
  if game and game.over then return end

  -- REPLACE the velocity rather than adding to it, so every flap is identical
  -- whatever you were already doing. That is what makes it learnable.
  if input.justPressed("Jump") then
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
  local game = findScript("flappyGame")
  if game then game.lose() end
end
```

*Done when: Bird runs flappyBird.lua.*

## 3. Build one pipe, then make it a prefab

Add an **Empty** node called `Pipe`. Under it (drag them onto its row in the
Hierarchy to parent them) add two **Cubes**: one stretched upward above the gap,
one below it. Give each cube a **Rigidbody** with **mode** **Static** — that is what the bird
will hit.

Then drag the `Pipe` node from the Hierarchy into the **Assets** panel. That
writes `prefabs/Pipe.prefab.ron`.

### What a prefab is, and why this game needs one

A prefab is a saved node and everything under it — the shapes, the bodies, the
scripts, the settings. `spawn("Pipe", position)` stamps out a fresh copy while
the game runs.

You need it here because a script can *create* nodes but can't give them
colliders. Anything that has to be solid must be authored once and spawned.
That's not a limitation you'll fight; it's the normal way to make bullets,
enemies, debris and pipes.

Once the prefab is saved, **delete** the `Pipe` node from the scene. The spawner
will make its own.

*Done when: prefabs/Pipe.prefab.ron exists.*

## 4. Make the pipe move and score

Create the script below and attach it to the `Pipe` **prefab** — select it in
Assets and attach there, or re-open the prefab, add the script, and save it
again.

### Every pipe cleans up after itself

    if node.x < -params.behind then
      node:destroy()
    end

The alternative is a spawner holding a list of every pipe it ever made,
remembering to walk it, and leaking the ones it forgets. Letting each object
decide when it is finished is smaller, and it stays correct when something
destroys a pipe for a reason the spawner never hears about.

### Scoring on the pipe, not on the bird

The pipe knows when it has passed x = 0. The bird would have to check every pipe
every frame to work the same thing out. Put the decision where the information
already is — that principle will save you more code than any other one here.

The `scored` flag is what stops one pipe scoring sixty times a second while it
crosses the line.

`scripts/flappyPipe.lua`

```lua
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
  local game = findScript("flappyGame")
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
end
```

*Done when: scripts/flappyPipe.lua exists.*

## 5. The rules

Add an **Empty** node called `Game` and attach the script below.

Press Play. Pipes arrive, the score counts, hitting one ends it, and Space
starts again.

### Spawning on a schedule

    spawner = every(params.interval, function()
      ...
    end)

`every` repeats on the game clock and hands back a handle you can `:cancel()`.
Doing this with a countdown in `update` is four more lines and one more thing to
get wrong; more importantly, `every` pauses when the game does.

Cancelling it in `lose()` is what stops pipes marching on over the game-over
screen.

### A published variable

    over = false

Declared with no `local`, on purpose. The bird and the pipes read
`game.over` through their script handles. Locals are private to a file; this one
is the script's public state, and that difference is worth being deliberate
about.

### Restarting

    scene.load(scene.current())

Reloading the current scene is a complete reset: every node back to its authored
state, every script's `start()` run again. It's the cheapest possible restart
and it is exactly right for an arcade game.

Note the high score goes through `save.*`, so it survives the reload — and
quitting, and the exported build.

`scripts/flappyGame.lua`

```lua
-- The rules: spawn pipes, keep score, end the run, start it again.
--
-- Put this on an Empty node. It is the only script that knows the game is a
-- game — the bird just flaps, the pipes just drift.

defaults = {
  --@desc Prefab spawned as an obstacle.
  pipe = "Pipe",
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
  best = save.get("best", 0)

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
    save.set("best", best)
  end
end

function update(node, dt)
  if not camera.exists() then return end
  local w, h = camera.screenSize()

  draw.text(w * 0.5, 40, tostring(points), 48, 1, 1, 1, 1, "center")

  if over then
    draw.text(w * 0.5, h * 0.5 - 20, "Game over", 40, 1, 0.5, 0.45, 1, "center")
    draw.text(w * 0.5, h * 0.5 + 30, "Best " .. best .. " — Space to try again",
      22, 1, 1, 1, 0.8, "center")

    -- Reloading the scene is a complete reset: every node back to how it was
    -- authored, every start() run again.
    if input.justPressed("Jump") then
      scene.load(scene.current())
    end
  end
end
```

*Done when: Game runs flappyGame.lua.*

## 6. Tune it until it's actually fun

This is the real work, and it is entirely done from the Inspector with the game
running. Nothing below needs a code change.

- **Flap** on the bird, and **Interval** and **Gap** on the game, are the three
  numbers that decide whether this is playable. Move one at a time.
- If it's too hard, widen the gap before you slow the pipes down — a slow game
  is boring in a way a hard one isn't.
- Make the gap narrow as the score climbs. Two lines in `score()`.

Press Play and get a score you're pleased with before moving on. Playing your
own game for five minutes will tell you more than reading about game feel for an
hour.

*Done when: you've pressed Play.*

## 7. Make it a real game

It's finished. Everything from here is polish — which is most of what separates
a project from a game.

### Cheap wins, roughly in order

- **Sound.** A flap, a score blip, a crash. The **≣ Mixer** tab, and
  `node:sound()`.
- **A particle burst on death** — the **✱ Particles** tab, then
  `spawnEffect("Crash", node.pos)`.
- **A start screen.** `over = true` at the beginning and a different message
  until the first Space.
- **Something behind it.** A few slow-moving background shapes at negative z
  give an enormous amount of depth for almost nothing.
- **Ship it.** **File → Export Game** builds a standalone executable for
  Windows, macOS or Linux. This is a small enough game to actually finish and
  hand to someone, which is a rare and worthwhile thing.

### And then

The **3D platformer** tutorial covers cameras, moving platforms and level
structure; the **top-down RPG** covers state that outlives a scene. Between the
three you have seen most of the engine's shape.

The finished version of this one is the `flappy` starter template.

