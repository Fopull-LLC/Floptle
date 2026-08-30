# Make it multiplayer

The platformer, played by two machines at once — and everything that has to change for that to work.

**some coding** · about 50 minutes · 11 steps

> Follow this along **inside the editor** — the 🎓 Learn tab has the same steps and ticks each one off as your project starts to match it.

The platformer you built, played by two machines at once — one hosting the
truth, the other seeing it a beat later — and everything that has to change for
that to work. Which is less than you'd think: a component on the things that
move, one line to host, and two scripts that learn to say "the server decides".

You'll test all of it on one desk. The editor can run a real client against a
real server inside one process, over a link whose latency and packet loss you
set with a slider — so every bug that only shows up at 200 ms is one you find
now, alone, rather than in front of the first person who plays it with you.

## What you should already know

The **Build a 3D platformer** tutorial, or its starter template open in front of
you. This picks up exactly where that ends.

## The plan

1. Decide what is shared, and mark it.
2. Host a session from the game script.
3. Join a ghost client, and make the link bad on purpose.
4. Put the score and the coins under the server's control.
5. A door you ask the server to open.
6. Make your own character predicted, and feel the difference.

## 1. Start from the platformer

Open the finished platformer: create a new project with the **platformer**
template (in the Hub, or `floptle --new <dir> --template platformer`), or use the
one you built in that tutorial.

Press Play once to make sure it runs: walk, jump onto the `Lift`, collect a
coin. Everything below assumes the nodes that tutorial made — `Ground`,
`Player`, `Camera`, `Platform`, `Lift`, `Game`, some `Coin`s and a `Goal` — and
that `Player` is tagged `player`.

Nothing about the level itself changes. Multiplayer in this engine is something
you add to a game, not something you rebuild it around.

*Done when: a node called Player is in the scene.*

## 2. What a session is

One machine is the **host**. It runs the real game — the one where the coin was
or wasn't collected, where the lift is right now — and that is the truth.
Everyone else is a **client**: it draws what the host tells it, about a tenth of
a second late, and when a client wants something to happen it *asks*. That's
the whole model, and the reason it's the default is that it makes cheating a
matter of asking the server nicely.

### What gets shared

Only what you mark. A node with a **🌐 Networked** component exists on every
machine and the host keeps them agreeing; a node without one is local, which is
the right answer for the camera, the UI, particles, and scenery that never
changes.

The rule of thumb for what to sync on a networked node:

- **sync transform** for things physics moves — the player, the lift.
- **only vars** (transform off) for things a script animates from a fact — a
  door that eases open because `open` is true, a coin that hides because
  `taken` is true. The script runs on every machine; the fact is what travels.
- **nothing** for the camera. Every machine has its own.

### Which scripts run where

In a session every script runs on every machine — with one exception: a node
whose transform the server owns is driven entirely by the server's snapshots
on a client, so its scripts don't run there. Everything else does, and
`net.isServer()` is how a line of code says "only the one with the truth does
this". You'll write that line four times in this tutorial.

## 3. Mark what moves

Select `Player`. In the Inspector, **➕ Add Component → 🌐 Networked**. Leave
**mode** on **Server authority** and **sync transform** ticked, and tick
**sync physics** too — a rigidbody's velocity lets a client extrapolate between
snapshots instead of stepping, and you'll want it on for the last step.

Do the same for `Platform` and `Lift`, minus sync physics: they're kinematic, so
the transform alone says everything.

Leave `Ground`, `Camera`, the coins and the `Goal` alone for now. The ground
never moves. The camera is *yours* — the other machine has its own, following
its own player. The coins are the next lesson.

### Why the default is the right default

**Server authority** means the host simulates this node and clients receive
it. It costs nothing from your scripts — the movement code you already wrote
just runs on the host — and it's cheat-proof, because nothing a client sends
about its own position is believed. Start every node here. Change it only for
the one node the local player controls, and only when the input lag becomes the
thing you hate about your game.

*Done when: Player has a Networked component.*

## 4. Host from the game script

Replace `platformerGame.lua` with the version below. Three things changed.

### net.host

    net.host{ maxPlayers = params.maxPlayers }

No port, no relay — that is the **in-editor harness**, and it's what the next
step uses. Later, `net.host{ relay = "…" }` is the one-line difference between
this and playing across the internet.

### The respawn is the server's

    if net.isServer() and player.y < params.fallY then

On a client, `Player` is a copy driven by the server's snapshots. Teleporting it
from the client's own `update` would be overwritten a moment later — and it
would mean two machines disagreeing about where the player is, which is the
one thing a session exists to prevent. Facts about *where things are* are
decided in one place.

### The HUD says who you are

`net.role()` reads `"server"`, `"client"` or `"offline"`. In a session this
script runs on every machine, and while you're learning, a line on screen that
says which one you're looking at is worth more than any amount of reasoning.

`scripts/platformerGame.lua`

```lua
-- The script that knows about the GAME rather than about a node: the score,
-- falling off the world, whether you have won — and now, the session.
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
  --@desc How many can be in the session, you included.
  --@range 1 16
  maxPlayers = 4,
}

local score = 0
local won = false
local player

function start(node)
  player = find("Player")
  score = 0
  won = false

  -- Become the host. No port and no relay means the in-editor harness:
  -- press 🌐 in the toolbar to join a ghost client. A relay address here
  -- is the only difference between this and playing across the internet.
  net.host{ maxPlayers = params.maxPlayers }
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
  -- Only the host decides where the player IS. On a client the Player is a
  -- copy driven by the server's snapshots, and moving it here would be
  -- overwritten a moment later anyway.
  if net.isServer() and player and player.valid and player.y < params.fallY then
    player.pos = vec3(0, params.spawnY, 0)
    player.vel = vec3(0, 0, 0)
  end

  if not camera.exists() then return end
  local w, h = camera.screenSize()

  draw.text(24, 24, "Coins: " .. score, 24, 1, 0.85, 0.35)

  -- Which machine this is. In a session every script runs everywhere, and
  -- this is the line that tells you which "everywhere" you are looking at.
  local who = net.role()
  if net.isServer() then who = who .. " · " .. #net.peers() .. " client(s)" end
  draw.text(24, 56, who, 18, 0.7, 0.9, 1)

  if won then
    draw.text(w * 0.5, h * 0.5, "You made it!", 44, 1, 1, 1, 1, "center")
  end
end
```

*Done when: platformerGame.lua hosts a session.*

## 5. Join a ghost, then ruin the connection

Press Play. The HUD reads `server · 0 client(s)` — your script is hosting.

Click the **🌐** button in the toolbar. The panel says *hosting · 0 ghost
clients*; click **➕ Join a local ghost client**. A real client has joined your
real server, inside this one process, over a simulated link.

**Cyan spheres** appear: one per networked node, each drawn where the *client*
believes that node is. Walk around. Yours trails a beat behind you. The lift's
ghost rides the lift. Nothing else has a ghost — because nothing else is
networked, which is exactly the point of the previous step.

### Now make it bad

In the same panel, drag **latency (ticks)** up to 12 — about 200 ms round
trip, a bad day between two countries — and **packet loss** up to a tenth. Watch
your ghost fall further behind and start to stutter. That is what the other
player sees of you, and it is what every decision in the next steps is about.

Do this early and often. A bug that only appears at 200 ms is one you want to
find on your own desk, with the slider in your hand, rather than in a voice call
with someone saying "it's weird on my end".

*Done when: you've pressed Play.*

## 6. The game itself is networked

Right now the score is a `local` in the `Game` script. On the client, the same
script runs with its *own* `score`, which stays at zero forever — and someone who
joins after you've collected six coins sees nothing about them.

The score isn't a fact about a coin, or about the player. It's a fact about the
**game**. Those live on the `Game` node, and they have to travel.

Select `Game` and **➕ Add Component → 🌐 Networked**. Then **untick sync
transform**: nothing about this node moves, and a networked node that syncs
only vars runs its script on every machine — which is what lets each machine's
HUD read the numbers.

That's the whole step. The script that uses it is next.

*Done when: Game has a Networked component.*

## 7. Move the score into synced

Replace `platformerGame.lua` again, with the version below.

### replicated and synced

    replicated = { score = 0, won = false }

A top-level `replicated` table declares which of this script's values travel,
with their starting values. You then read and write them through `synced`:
`synced.score`, `synced.won`. Numbers, booleans, strings and small tables are
all fine.

### Only the server writes

    if net.isServer() then synced.score = synced.score + 1 end

Only the host's writes replicate. A client writing to `synced` gets a warning in
the Console and is overwritten by the next snapshot — so the guard isn't
defensive, it's the rule made visible. `collect()` runs on whichever machine's
coin trigger fired; only the server's call counts.

### Late joiners

Everything in `synced` is part of the join handshake. Stop, press Play, collect
three coins, *then* join the ghost: its copy of the script starts at `score =
3`. You didn't write anything for that. That is what "the server has the
truth" buys you.

`scripts/platformerGame.lua`

```lua
-- The script that knows about the GAME rather than about a node: the score,
-- falling off the world, whether you have won — and the session.
--
-- Put it on an Empty node WITH a Networked component (sync transform off:
-- nothing here moves, only facts). Everything else reaches it with
-- findScript("platformerGame") and calls the functions below.

defaults = {
  --@desc Fall below this height and you are put back at the start.
  --@units m
  fallY = -20,
  --@desc Where the start is.
  --@units m
  spawnY = 3.0,
  --@desc How many can be in the session, you included.
  --@range 1 16
  maxPlayers = 4,
}

-- Facts about the GAME live in synced, not in locals: every machine reads
-- the same numbers, and someone who joins ten minutes in gets the current
-- ones in the handshake.
replicated = { score = 0, won = false }

local player

function start(node)
  player = find("Player")
  net.host{ maxPlayers = params.maxPlayers }
end

-- Called by coin.lua. Only the server's writes to synced replicate — a
-- client's would be overwritten, with a warning — so the guard is the rule
-- "the server decides", written down.
function collect()
  if net.isServer() then synced.score = synced.score + 1 end
end

-- Called by goal.lua.
function reach()
  if net.isServer() then synced.won = true end
end

function update(node, dt)
  if net.isServer() and player and player.valid and player.y < params.fallY then
    player.pos = vec3(0, params.spawnY, 0)
    player.vel = vec3(0, 0, 0)
  end

  if not camera.exists() then return end
  local w, h = camera.screenSize()

  draw.text(24, 24, "Coins: " .. synced.score, 24, 1, 0.85, 0.35)

  local who = net.role()
  if net.isServer() then who = who .. " · " .. #net.peers() .. " client(s)" end
  draw.text(24, 56, who, 18, 0.7, 0.9, 1)

  if synced.won then
    draw.text(w * 0.5, h * 0.5, "You made it!", 44, 1, 1, 1, 1, "center")
  end
end
```

*Done when: platformerGame.lua declares the score as a synced value.*

## 8. Coins everyone agrees on

The coin still calls `node:destroy()` on whichever machine's trigger fired. The
host takes a coin; the client's copy of it is still floating there, because
nobody told it otherwise — and nothing ever could tell a machine that joins
later about a node that no longer exists.

Select every `Coin` (click the first, shift-click the last) and **➕ Add
Component → 🌐 Networked**, then **untick sync transform** on them. A coin never
moves; what has to be shared is whether it's been taken, and that's one flag.

Then replace `coin.lua` with the version below.

### Hidden, not destroyed

    node.visible = not synced.taken

A destroyed node has no way to say it's gone. A synced flag does — it is
carried to every machine now, and to anyone who joins later. The trigger fires
on every machine the player's body passes through it on; the server's is the
one that counts, and everyone else hears about it through `synced`. Notice
that this is the *same* shape as the score — a fact, decided in one place,
read everywhere.

Press Play, join a ghost, and collect a coin. It vanishes; the score goes up
once, not twice.

`scripts/coin.lua`

```lua
-- A pickup everyone agrees on. The node needs a collider with `trigger`
-- ticked, and a Networked component with sync transform OFF: a coin never
-- moves, so its transform has nothing to say — what has to be shared is
-- whether it has been taken, and that is one synced flag.

replicated = { taken = false }

function onTriggerEnter(node, other, hit)
  if synced.taken then return end
  if not other:hasTag("player") then return end
  -- The trigger fires on every machine the body passes through it on. Only
  -- the server's answer counts; everyone else hears about it through synced.
  if not net.isServer() then return end

  local game = findScript("platformerGame")
  if game then game.collect() end
  synced.taken = true
end

function update(node, dt)
  -- Hidden rather than destroyed: a destroyed node can't tell someone who
  -- joins later that it's gone. A synced flag can.
  node.visible = not synced.taken
end
```

*Done when: coin.lua marks the coin taken through synced.*

## 9. A door you ask the server to open

Everything so far has been the server *noticing* things. Now a client wants
something.

Add a **Cube**, name it `Door`, scale it to about `3, 4, 0.5`, and stand it across
a gap in your level. **Rigidbody**, mode **Kinematic** — it's solid, and the
script moves it. **🌐 Networked**, with **sync transform unticked**: the script
animates this node from one fact, on every machine.

Attach `netDoor.lua`. Press Play, walk up to the door and press **E**
(`Interact` — a named action every project starts with).

### What an RPC is

    net.rpc("use")

That's a request, from whichever machine the player is on, to the server:
"I'd like the door toggled". It arrives as `onRpc.use(args, sender)` on the
server, `sender` is the peer that asked (verified — a client can't claim to be
someone else), and the server decides. What it decides goes into `synced.open`,
and every machine — including the one that asked — eases the door from that.

Turn latency up to 12 ticks and press E again. There's a beat between the key
and the door moving: the round trip. That's honest, and it's the same for
everyone — nobody's door opens early because they're the host. Fine for a door.
Wrong for your own legs, which is the last step.

`scripts/netDoor.lua`

```lua
-- A door the server opens when a player asks. Attach to a solid node — a
-- Rigidbody in KINEMATIC mode — with a Networked component, sync transform
-- OFF: this script moves the door on every machine from one synced flag.

defaults = {
  --@desc How far the door lifts when open.
  --@range 0 10 --@units m
  lift = 3.5,
  --@desc How close the player has to be to use it.
  --@range 0 10 --@units m
  reach = 3.0,
}

replicated = { open = false }

local player
local restY

function start(node)
  player = find("Player")
  restY = node.y
end

-- An RPC is an INTENT: "I'd like the door toggled". The server decides, and
-- the decision reaches everyone — including this machine — through synced.
onRpc = {}
function onRpc.use(args, sender)
  if net.isServer() then synced.open = not synced.open end
end

function update(node, dt)
  if player and player.valid and input.justPressed("Interact")
     and distance(player, node) < params.reach then
    net.rpc("use")
  end

  -- Cosmetic, and on every machine: ease toward what the server says.
  local target = restY + (synced.open and params.lift or 0)
  node.y = ease(node.y, target, 6, dt)
end
```

*Done when: Door runs netDoor.lua.*

## 10. Make your own character predicted

With latency at 12 ticks, walk. Your character responds a fifth of a second
after you press the key, because under **Server authority** your input goes to
the server and the result comes back. For a door that's fine. For the one node
you are steering, it's the thing that makes an online game feel broken.

Select `Player` → **🌐 Networked → mode → Predicted (owner)**. (`sync physics`
is already on, from step 3 — prediction needs the velocity.)

Press Play, open **🌐**, and this time click **🎮 Test as remote player
(predicted)**. Your play world becomes a *client*, predicting against a hidden
authoritative server; the **orange ghost** is the server's truth. Drag latency
to 12. Your character stays glued to your keys while the ghost lags behind —
and when the two disagree, you're smoothly corrected toward it.

Watch the `reconciles` line in the panel. **Corrections near 0% is healthy.** A
rate that climbs means the client is doing something in `fixedUpdate` the server
can't reproduce.

### Why it just worked

You didn't touch the movement script. Two rules from the platformer tutorial —
**gameplay in `fixedUpdate`**, and **named actions, never raw keys** — turn out
to have been the prediction contract all along. The server re-runs your exact
inputs through the exact same code at the exact same rate, so it gets the same
answer. A script that polled `input.pressed("space")` would read neutral on a
predicted node and simply never jump, with no error anywhere — which is why the
Input settings list every such call site.

## 11. Where to go next

You have a networked platformer, tested at 200 ms and 10% loss without leaving
your desk. The things you'd reach for next, in the order you'll want them:

- **Two real machines.** Both open this project and press Play. The host
  clicks **🌐 → ⏵ Host — get a lobby code** and reads out five letters; the
  other types them into **code → ⏵ Join by code**. Nobody port-forwards — the
  relay is a rendezvous, and you can run your own with `cargo run -p
  floptle-relay` on any box both machines can reach. From a script, it's the
  one-line change promised in step 4: `net.host{ relay = "host:7788" }`, and
  `net.lobbyCode()` to put the letters on your own lobby screen.
- **A lobby screen that handles a wrong code.** `net.join` doesn't block; wait
  on `net.joinState()` and show its reason. Mistyping the code is the most
  common thing that will ever go wrong in an online session.
- **One avatar per player.** Instead of an authored `Player`, spawn one for
  each joiner with `net.spawn` on `playerJoined` — see *Per-player avatars* in
  `docs/scripting.md` §16.
- **Hits that respect latency.** `net.rewind` judges a swing against the world
  the attacker was actually seeing.
- **Rollback**, if you're making a fighting game — and only then.

`docs/multiplayer.md` is the long version of all of it, in that order.

