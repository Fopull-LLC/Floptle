# First steps — make something move

A cube you spin, tune and drive around, in about fifteen minutes.

**no experience needed** · about 15 minutes · 8 steps

> Follow this along **inside the editor** — the 🎓 Learn tab has the same steps and ticks each one off as your project starts to match it.

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

## 1. Look around

Click into the **⌖ Scene** panel and hold the **right mouse button**. Now:

- **W A S D** — fly forward, left, back, right
- **mouse** — look
- **Q / E** or **Ctrl / Space** — down and up

That's the editor's camera, not a game camera. It exists so you can get a look
at what you're building, and it has nothing to do with what a player will see.

Take thirty seconds to fly around the starter scene. The crate, ball and capsule
in front of you are ordinary nodes with physics on them — press **⏵ Play** and
they'll fall. Press it again to put them back.

## 2. Add a cube and name it

In the **Hierarchy** panel (top left), open the **✚ New** menu and pick
**■ Cube**. It appears at the origin.

Now rename it. Double-click its row in the Hierarchy, type `Spinner`, press
Enter.

### Why the name matters

It isn't decoration. Scripts find nodes by name — `find("Spinner")` — and so
does this tutorial: the tick beside this step appears when a node called
`Spinner` exists. Get in the habit of naming things the moment you make them.
The alternative is a scene of eleven nodes called Cube.

*Done when: a node called Spinner is in the scene.*

## 3. Write your first script

Press **Create scripts/spinner.lua** below. That writes the file and opens it in
the **Scripting** tab.

### Reading it

`function update(node, dt)` declares a **hook** — a function the engine calls
for you. `update` runs once for every frame drawn, which on most machines is
somewhere between 60 and 240 times a second.

The engine hands it two things:

- `node` — the node this script is running on. Not "the cube"; whichever node
  it happens to be attached to. That's why one script can spin twenty things.
- `dt` — how many seconds the last frame took. A small number, around 0.016.

`node.yaw` is how far the node is turned around the vertical axis, in radians.
`math.rad(90)` is ninety degrees expressed in radians.

### The one line worth understanding properly

    node.yaw = node.yaw + math.rad(90) * dt

Multiplying by `dt` is what makes it ninety degrees **per second** rather than
ninety degrees **per frame**. Without it, the cube spins nearly four times
faster on a 240 Hz monitor than a 60 Hz one — the classic bug that makes a game
feel different on someone else's computer. Any time you add to something every
frame, multiply by `dt`.

`scripts/spinner.lua`

```lua
-- Turns this node ninety degrees a second, forever.

function update(node, dt)
  node.yaw = node.yaw + math.rad(90) * dt
end
```

*Done when: scripts/spinner.lua exists.*

## 4. Attach it to the cube

A script that isn't attached to anything does nothing at all — this is the step
people miss.

Select `Spinner` in the Hierarchy. In the **Inspector** (on the right), press
**➕ Add Component** and pick `spinner` from the **Scripts** group.

Or: drag `spinner.lua` from the **Assets** panel straight onto the node's row in
the Hierarchy. Same result.

*Done when: Spinner runs spinner.lua.*

## 5. Press Play

Hit **⏵ Play** in the toolbar (or press **F1**).

The cube turns. Press Play again to stop, and note that it snaps back to where
it started — play mode never changes your scene, so you can experiment with
absolutely no risk.

### If nothing happens

- Check the **Console** panel. A script with a mistake in it reports the file
  and the line, and keeps the rest of the game running.
- Check the script is actually attached (previous step), and that its checkbox
  in the Inspector is ticked.

*Done when: you've pressed Play.*

## 6. Make the speed adjustable

Right now ninety degrees is baked into the code. Changing it means editing,
saving, and playing again — a slow loop for something you want to *feel* your
way to.

Add a `defaults` table and the Inspector builds a row for every value in it.
Open `scripts/spinner.lua` and replace what's there with the version below.

Now select `Spinner`, press Play, and **drag the Speed slider while the game is
running**. The cube responds immediately.

### What just happened

- `defaults` declares the script's tunables. Anything you put in there shows up
  in the Inspector, in the order you wrote it.
- `params.speed` reads the current value — the one in the Inspector, not the
  one in the file. The default is only the starting point.
- The `--@` comments describe the row: `--@range` bounds it, `--@units` puts a
  suffix on the number, `--@desc` becomes its tooltip. They're comments, so
  nothing breaks if you delete them or spell one wrong.

This is the single most useful habit in the whole engine. A number you can drag
while the game runs is worth ten you have to guess at.

`scripts/spinner.lua`

```lua
-- Turns this node, at a speed you can change while the game is running.

defaults = {
  --@desc How fast it turns.
  --@range 0 720 --@units deg/s
  speed = 90,
}

function update(node, dt)
  node.yaw = node.yaw + math.rad(params.speed) * dt
end
```

*Done when: spinner.lua reads params.speed.*

## 7. Drive it with the keyboard

Add movement. Replace `spinner.lua` with the version below and press Play —
**W A S D**, or a gamepad stick, now pushes the cube around.

### Ask for actions, not for keys

    local x, y = input.axis2("Move")

`Move` is a **named action**, defined in **⚙ Settings → Input** and already
bound to W A S D *and* the left stick. Asking for `Move` rather than for the W
key buys you three things without any extra work: gamepads, players who want to
rebind their controls, and code that still reads sensibly in a year.

`x` is -1 to 1 left-to-right, `y` is -1 to 1 back-to-forward. On a stick they're
smoothly in between.

### Why forward is minus Z

    node.z = node.z - y * params.nudge * dt

In Floptle, +X is right, +Y is up, and forward is **-Z**. So pushing the stick
forward should *decrease* z, which is where that minus comes from. It catches
everyone once.

`scripts/spinner.lua`

```lua
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
  local x, y = input.axis2("Move")
  node.x = node.x + x * params.nudge * dt
  node.z = node.z - y * params.nudge * dt
end
```

*Done when: spinner.lua asks the input map which way you're pushing.*

## 8. Where to go next

You've now done the four things the rest of it is made of: made a node, given it
a behaviour, exposed a number, and read the player's input.

### Try breaking it, on purpose

- Take the `* dt` out and watch the speed become a property of your monitor.
- Set Speed to 0 and drive around. Set it to 720.
- Change `node.yaw` to `node.y` and see what "turning" becomes.
- Attach `spinner` to the ball as well. One script, two nodes, separate
  Inspector values on each.

### Then pick a game

**Build Flappy** is the shortest — one button, one rule, and a finished game at
the end. **Build a 3D platformer** is the natural next one if you'd rather run
and jump around. Both assume only what you just did.

The **⚙ API** page of the Scripting tab lists every call the engine has, with an
example for most; the search there is the fastest way to answer "what was that
called?".

