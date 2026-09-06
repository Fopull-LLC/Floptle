# Making a multiplayer game

This is the **build-something** guide. It goes from a single-player scene to two
machines playing together, then to shipping. For *why* any of it works the way it
does, the reasoning is recorded in the decision records —
networking and cloud (ADR-0022) and
rollback netcode (ADR-0025). For the full API
reference, [scripting.md §16–16b](scripting.md).

Everything here is one netcode with three replication modes. You pick the mode
per node, in the Inspector, and a project that doesn't use a mode never pays
for it.

---

## 1. Pick the mode first

This is the only decision that is expensive to change later, so make it
deliberately. It is a property of your **game**, not of your network.

| Mode | Who simulates | Use it for | Cost |
|---|---|---|---|
| **Server authority** | the host only | almost everything: doors, pickups, NPCs, projectiles, scenery | clients render ~100 ms behind |
| **Predicted (owner)** | host **and** the owning client | a player's own avatar in a shooter/action game | the owner sometimes gets corrected |
| **Rollback (all peers)** | **every** peer, every tick | fighting games, and only fighting games | every gameplay value must be snapshottable |

The short version:

- **Start with Server authority.** It is the default, it is cheat-proof, and it
  needs nothing from your scripts.
- Add **Predicted** to the one node the local player controls, when the input
  lag becomes the thing you hate about your game.
- Choose **Rollback** only if your game is decided by what the *opponent* is
  doing on this exact frame. It is the strictest contract in the engine and
  §4 below is entirely about paying it.

Mixing is fine and normal. A fighting game with a rollback-driven pair of
fighters can still have an `Authority` stage hazard and a `Predicted` spectator
camera.

---

## 2. The five-minute version (server-authoritative)

**In the scene**

1. Select the node that should exist on every machine.
2. **Add Component ▸ 🌐 Networked**.
3. Leave `mode` on **Server authority**. Check `sync transform`. Check
   `sync physics` if it has a Rigidbody, `sync animator` if it has an Animation
   Controller.

Nodes *without* this component stay local — that's the default, and it's the
right one for particles, cameras, UI and scenery.

**In a script**

```lua
-- Anywhere; a lobby node is a good home.
function start(node)
  net.host{ maxPlayers = 4 }        -- no port/relay = the in-editor harness
end
```

**Test it**

Press **⏵ Play**, then the **🌐** button in the toolbar → **⏵ Host + join a
local client**. You now have a real client talking to a real server inside one
process, over a link whose latency and loss you control with the sliders. Turn
latency up to 12 ticks (≈200 ms) and loss to 10% early and often — a bug that
only appears at 200 ms is a bug you want to find on your own desk.

**Make something happen**

```lua
replicated = { open = false }        -- declares a synced variable

onRpc = {}
function onRpc.use(args, sender)
  if net.isServer() then synced.open = not synced.open end
end

function update(node, dt)
  local target = synced.open and 1.6 or 0.0
  node.y = node.y + (target - node.y) * math.min(1, dt * 6)
end
```

The client sends an intent (`net.rpc("use")`), the **server** decides, and the
result reaches everyone — including someone who joins ten minutes later, because
`synced` state is part of the join handshake. That is the whole shape of
server-authoritative multiplayer, and most of your game can be written this way.

---

## 3. Adding prediction

A player's own avatar shouldn't wait for a round trip. Set that node's Networked
mode to **Predicted (owner)** and check `sync physics`.

Nothing else changes: the same `fixedUpdate` runs on the owner's client *and* on
the server, the server's result wins, and when they disagree the client is
smoothly corrected. Your script does not need to know which machine it is on.

Test it with **🎮 Test as remote player (predicted)** in the 🌐 panel. Your play
world becomes a client predicting against a hidden authoritative server, and the
orange ghosts are the server's truth. Drag the latency slider up: your character
should stay locked to your input while the ghosts drift.

Watch the `reconciles` line in the panel. **Corrections near 0%** is healthy. A
correction rate that climbs with nothing else changing means the two simulations
disagree — usually because the client is doing something in `fixedUpdate` the
server can't reproduce.

### Hit detection that respects latency

Your client shot at where the target *was* on its screen. The server's clock says
that was 80 ms ago. `net.rewind` judges the shot against the world the shooter
actually saw:

```lua
-- client: stamp the intent with the tick you were SEEING
net.rpc("swing", { dx = dx, dz = dz }, { withInput = true })

-- server: judge it in that moment
function onRpc.swing(args, peer)
  if not net.isServer() then return end
  net.rewind(peer, function()
    local hit = raycast(node.x, node.y, node.z, args.dx, 0, args.dz, 3.0)
    if hit then log("hit " .. hit.node.name) end
  end)
end
```

Inside the closure, raycasts and other scripts' `synced` vars read the rewound
tick — so a parry that was up on the attacker's screen counts.

---

## 4. Rollback (fighting games)

Every peer simulates every fighter, every tick, from the shared input set.
Nothing about a hit crosses the wire — only inputs do — so both machines agree
about hitstop, meter and trades because the *simulation* agrees.

### Setup checklist

1. **Networked ▸ mode ▸ Rollback (all peers)** on each fighter.
2. **Settings ▸ Input ▸ Local players** = the number of fighters. This is the
   step people miss: every Rollback node reads its own input slot whether the
   opponent is on the same couch or another continent. Left at 1, fighter #2
   stands still all match. (The engine raises a Console fault at match start if
   these don't match — but read it here first.)
3. On each fighter's Rigidbody, turn on **pushbox only**. The contact solver is
   the part least likely to agree bit-for-bit between two machines; with this on,
   the body integrates velocity and nothing else, and your script owns gravity,
   the floor and pushout — which is how the genre works anyway.
4. Give every gameplay script on those nodes `snapshot()` and `restore()`.

### The contract

```lua
local state, frame, health = "idle", 0, 100

function snapshot()
  return { state = state, frame = frame, health = health }
end

function restore(s)
  state, frame, health = s.state, s.frame, s.health
end
```

When a remote input contradicts what was predicted, the engine restores the last
agreed tick and re-simulates everything since, with no rendering in between —
putting your script back through these two hooks.

> **Anything you leave out of `snapshot()` survives the rewind unchanged.**
> That is precisely what a desync is made of. Nothing warns you at the moment
> you forget: the match keeps playing and the two machines quietly stop
> agreeing. If a value affects what happens, it belongs in `snapshot()`.

Transforms and physics bodies are saved for you — don't list them. A script that
defines neither hook is not rolled back at all, which is right for a cosmetic
and wrong for anything else.

### The four rules that actually bite

| Rule | Why |
|---|---|
| Read input as **actions** (`input.action`, `input.justPressed`, `input.axis1`), never raw keys | The wire carries *actions*. `input.pressed("j")` reads neutral on a networked node — the character simply never attacks, with no error anywhere. Bind them in **Settings ▸ Input**. |
| Count **frames**, not seconds | `heldSecs` reads 0 on rollback slots — the wire carries actions, not durations. Integer frames re-simulate exactly. |
| Read and write **`node.tickPos`**, never `node.x`, inside `fixedUpdate` | `node.x` is the *interpolated render pose*. Writing it teleports the body onto its visual position: the model slides and the hurtbox doesn't follow. |
| **`net.random()`**, never `rng()` | An unseeded roll comes from the clock. Two peers drawing different numbers is a match that forks in two. |
| Projectiles live in **snapshot state**, not `spawn()` | A spawned prefab isn't rollback state, and one-shot spawns are suppressed during re-simulation. A fireball both machines must agree on is data in your controller. |

### Input delay

Rollback holds your own input for a few ticks so the opponent's has time to
arrive. Too low and their input lands *after* the tick that needed it — on
every tick, forever — so the driver guesses, is wrong, and re-simulates. The
fight stays identical on both machines and the checksum stays green; it just
costs five or six times the simulation work and feels like it.

The host picks it, once, at match start:

```lua
net.host{ inputDelay = 5 }     -- ticks, clamped to 6
net.setInputDelay(4)           -- between matches; a rematch, not a new lobby
```

**If you don't pick one, the host derives it from the worst peer's measured
RTT** — `ceil(one-way / tick) + 1`, which is 2 on a LAN and 5 across a country.
That is right far more often than the constant 2 it replaced.

It is **fixed for the session and never auto-adjusted mid-match**, deliberately.
Adaptive delay hides a bad connection by changing how the game feels while you
are playing it, and a fighting game cannot tolerate that. Put the number in
your lobby next to rounds and clock, with the ping on screen while the player
picks. 2 frames on a LAN, 4–5 between houses.

### Testing a rollback match

Local first: **⏵ Host + join a local client** with the latency slider at 6–8
ticks. Then rehearse at real conditions on one desk — see §7.

The 🌐 panel is your instrument:

- **corrections / depth last / max / avg** — a healthy match sits at *low average
  depth*. `max` is the worst moment; `avg` is the texture of the connection.
- **delay N · M% guessed** — the two numbers that only mean something together.
  A rollback session working perfectly and one badly misconfigured look
  identical from outside: both are correct, both agree, both pass the checksum.
  The misconfigured one just does several times the work. The line turns orange
  past 50% guessed, because that is a delay too low for the link, not a bad
  connection — see **Input delay** below.
- **⚔ ROLLBACK · waiting for input** (orange) — a stall. Past the depth cap the
  sim waits rather than guessing further, so the game runs slightly slow instead
  of teleporting the opponent. It recovers on its own. Show your own "connection
  trouble" banner off `net.stalled()`, because otherwise a stall is
  indistinguishable from a bad frame rate.
- **✔ checksums agree through tick N** — the thing you want to see. Checksums
  are mandatory and always on: every 30 confirmed ticks each peer hashes its
  state and the host compares.
- **frontier · confirmed C of S simulated (N ahead)** — `confirmed` is the
  newest tick every peer's *real* input is known for; everything past it was
  simulated from a guess and can still be corrected. When the gap reaches the
  depth cap the sim stalls, so **a gap pinned at the cap means somebody's input
  has stopped arriving**.
- **⚖ replay divergence at tick N — Player2/fighter/st.hitstop** (red) — the
  most useful line in the panel, and the one you hope never to see. Twice a
  second the driver re-simulates the last four ticks from its own state ring
  with *provably identical inputs*, and checks the world comes out the same.
  Anything that doesn't is a value your simulation reads that `snapshot()` does
  not carry — a Lua local cached across hooks, a value on another script, a
  global. No network condition explains it. It is **local**, not a desync:
  nothing has gone wrong between the peers yet, and this machine is about to be
  wrong on its own. Handle it with `net.on("replayDiverged", ...)`. On in the
  editor, off in a shipped build, forced either way with
  `FLOPTLE_ROLLBACK_AUDIT=1` / `=0`.
- **per-peer lines** (`host · frontier 412 · 3 tick(s) held`) — what each peer
  has confirmed, and how many of its inputs the host is still holding for it.
  Healthy peers hold a handful. **A peer whose frontier has stopped moving while
  its held count climbs is the starved one** — that is the readout that names
  the machine, and it turns orange when it happens.

If a match stalls for more than a second the Console says the same thing in
words, on both machines, once a second — so a two-machine test does not need
anybody watching the panel.

### When it says DESYNCED

The panel goes red and the Console names the tick. In order of likelihood:

1. **A gameplay value outside `snapshot()`/`restore()`.** By far the most common.
   Audit every `local` in every script on a rollback node.
2. **An unseeded `rng()`** somewhere in the simulation path.
3. **A read of `node.x` inside `fixedUpdate`** instead of `node.tickPos`.
4. Something read from the clock — `time`, `os.time()`, a frame-rate-dependent
   accumulator.

Two tools narrow it down:

- **The referee** (automatic, host-side) runs a second simulation at the
  *confirmed* frontier only. It never guesses and never rolls back, so it is
  never wrong — only behind. Every peer's checksum is judged against it, which
  is the difference between "someone is out of sync" and "**that machine** is".
- **Replays.** Every match writes its input log to `replays/` in your project.
  Inputs and the seed *are* the match, so a full set is kilobytes, and playing
  one back re-simulates rather than re-enacts. Enter Play on the match's scene,
  open 🌐, and click the replay: a desync that reproduces in a replay is a
  desync you can debug at your leisure.

---

## 5. Playing over a real network

Two machines, both running the same project.

**Via relay (recommended — nobody port-forwards).** The host clicks **⏵ Host —
get a lobby code** and reads out the five letters; the joiner types them into
**code** and clicks **⏵ Join by code**. From a script:

```lua
net.host{ relay = "relay.fopull.com:7788" }
net.join("relay://relay.fopull.com:7788/ABCDE")
```

Run your own with `cargo run -p floptle-relay` on any box both machines can
reach; it is stateless and forwards opaque bytes.

**Direct (LAN or a self-hosted box).** Host on a UDP port, joiner uses
`quic://ip:port`. Needs the port reachable.

Player slots are the scene's `Predicted`/`Rollback` nodes in order — #1 the
host, #2+ the joiners — or spawn one per joiner from a script.

### Showing the code on your own lobby screen

`net.lobbyCode()` returns it, so players never have to open the engine's panel:

```lua
function update(node, dt)
  local code = net.lobbyCode()
  find("CodeLabel").text = code or "getting a code…"
end
```

**Poll it, don't read it once.** It's `nil` until the relay answers — a round
trip after `net.host{relay=…}` — and `nil` for good on a client or a direct/LAN
host, where there's no code and joiners use the address.

### Handling a wrong code

`net.join` does not block. `net.role()` reads `"client"` from the frame you call
it, whether or not that lobby exists — so a lobby screen that trusts role
congratulates a player on joining nothing. Wait on `net.joinState()`:

```lua
local state, why = net.joinState()
-- "offline" | "connecting" | "joined" | "refused"
if state == "refused" then
  find("Error").text = why      -- "no lobby QK7RM", in the relay's own words
end
```

Mistyping the code is the most common failure in an online session, and it's the
one your players will hit. Note the difference between a relay that says **no**
(`"refused"`, with a reason) and one that is switched **off** (never answers,
stays `"connecting"`) — the second needs a timeout of your own.

### A joiner plays with their own controls

Two different things are called a "slot", and only one of them is about
hardware:

- The **roster slot** is which fighter a peer drives. Host 0, joiners 1+. It's
  the same everywhere, so fighter #2 is fighter #2 on both machines.
- The **device slot** is whose keyboard and which gamepad the input is read
  from, and which per-player bindings apply.

On a couch these are the same number. Over a network they aren't: a joiner is
roster slot 1, but they're sitting alone at their own machine as *its* player
one. The engine samples their own player-one bindings and applies the result to
their roster slot — so **you don't need a separate binding set for joiners**,
and the same controls work whether someone is player one or player two.

One case still needs care: with `Pad(id: Any)` — the default — a player with
**two controllers connected** may find the second one driving them. Pin it with
`Pad(id: Slot(0))` if that's a problem.

---

## 6. Shipping

**Peer-hosted** is the default and needs nothing extra: an exported build's F1
menu is the same host/join flow. See [export-builds.md](export-builds.md).

**Dedicated server** — for a world that has to stay up when the host closes
their laptop, and so the host isn't also a player with an unfair zero-latency
view:

```
floptle-runtime --server <project-dir> [--scene scenes/arena.ron]
                [--port 7777 | --relay host:port] [--tick 60]
                [--interest 150] [--budget 16384]
```

Same `World`, same `Sim`, same scripts, no window and no GPU. It hosts
`Authority` and `Predicted` sessions. It **refuses `Rollback` scenes** by
design — a rollback match has every peer simulating every tick, so its "host" is
a referee and a relay, and for a fighting game that is one of the players.

**Player slots on a dedicated server.** In a hosted session, authored
`Predicted` node #1 belongs to the host and #2 onward to joiners, because
somebody is sitting at slot #1's keyboard. A dedicated server has nobody — so
there it hands slots out **from #1**, in node order, as players join, and frees
them when they leave. Reserving #1 would leave a body in the world that no
client predicts and no input drives, and the first player to join would spectate
their own avatar.

Two rules keep that out of your way: a slot your own script assigns is never
reassigned, and a peer that already owns something is never handed a second.
Do it yourself with `net.setOwner(node, peer)` — which is also how a player who
dropped gets their *own* slot back on reconnect rather than whichever one
happened to be free:

```lua
net.on("playerJoined", function(peer)
  local slot = slotFor(peer)                  -- your own reconnect bookkeeping
  if slot then net.setOwner(slot, peer) end
end)

net.on("playerLeft", function(peer)
  local slot = slotFor(peer)
  if slot then net.setOwner(slot, nil) end    -- free the slot, keep the body
end)
```

**Or skip authored slots entirely.** `net.spawn` sends the whole subtree, so a
player rig — capsule, camera child, arms mesh, bone-attached item socket — goes
over the wire as one thing:

```lua
net.on("playerJoined", function(peer)
  net.spawn("mp/Survivor", { owner = peer, y = 2.5 })
end)
```

That is the shape to prefer: authored slots cap the lobby at whatever the scene
was built with, which for a ranked mode is a product limit set by a build step.
Only the root replicates; children are local nodes that follow it, and their
scripts run on every peer. `net.despawn(root)` takes the subtree away
everywhere, and a peer leaving despawns what it owned.

**Interest management** — the player-count feature. Off by default, because
below a few dozen players broadcasting is cheaper and simpler:

```lua
net.host{ interest = 150, interestBudget = 16384 }   -- metres, bytes/sec
```

Each client is then told about its own neighbourhood instead of the whole world,
inside a byte budget. **Nothing is dropped for good** — what doesn't fit accrues
priority and goes in a later snapshot. The 🌐 panel shows, per client, `sent of
relevant` and how many are waiting; a waiting count that never comes back down
means the budget is too small for the scene.

For the few things every player must agree on from anywhere — the match clock,
the objective, the boss — tick **always relevant** on that node's Networked
component.

### A radius is not a security boundary

Interest management was built to save bandwidth, and for an open-world game that
is all it needs to be. For a **hidden-role or competitive** game it is worth
being blunt about what a radius does and does not buy: a client that has been
told where everyone within 25 m is standing *knows* where they are. It does not
have to draw them. Every mitigation on the client — hiding the node, muting the
sound, not rendering the marker — is a setting a modified client turns back off,
and in a game where knowing someone's position is the whole contest, that is the
difference between a mode that can ship competitively and one that cannot.

Tightening the radius bounds the leak; it does not remove one. Two server-side
answers do, and they compose:

**Line of sight.** Opt in per session, on top of the radius, naming the layer
your walls are on:

```lua
net.host{ interest = 25, interestOcclusion = "Level" }
```

A node whose line to the client's avatar is blocked by that layer is not
relevant to it. Only that layer blocks — a trigger volume, a water surface and
another player's capsule are all "in the way" and none of them is a wall. Losing
sight is damped by a few snapshots, so a body behind a door frame does not
flicker; regaining it is **immediate**, because a player stepping out of cover
has to be there on the frame they step out. It costs one ray per candidate per
client per snapshot, which is why it is off by default.

**Your own rule.** `net.setRelevant(node, peer, bool)` decides one (client, node)
pair outright, and outranks both tests:

```lua
net.setRelevant(killer, survivorPeer, false)   -- withhold
net.setRelevant(objective, peer, true)         -- pin, at any distance
net.setRelevant(killer, survivorPeer, nil)     -- back to the tests
```

Two things it cannot do, on purpose: a client is always told about **its own
avatar** (prediction reconciles against it, so hiding it would produce a player
who cannot see themselves) and about anything flagged *always relevant*.

A pin holds **whether or not interest management is on.** With `interest` unset
the server still sends one snapshot to everybody, minus what each client has
been pinned away from — nothing else is culled, no radius applies, no budget is
spent. Turning on a hidden-role filter is not a decision to opt into distance
culling as well.

Losing relevance behaves as it always has — a scene-authored node stops being
updated and is sent in full on re-entry, never despawned. The 🌐 panel shows,
per client, how many nodes are being withheld and by which rule, so "is my
filter working" is a number rather than a hope.

What this is not: encryption, obfuscation, or a client-integrity check. It is
the narrow thing — the server decides who is told about what.

---

## 6b. Running a public server

A server anyone can reach needs two things a friends-and-a-lobby-code session
never did: to know **who** a peer is, and to be able to do something about it.

### Kicking

```lua
net.kick(peer, "griefing the objective")
```

Server only. The reason goes out **before** the link closes, so the removed
player's UI can say what happened rather than showing the generic "connection
lost" that every unexplained drop produces. On their machine
`net.on("kicked", fn)` fires with it; on everyone else's, `playerLeft` carries
it as a second argument. The roster entry goes immediately, so a client that
ignores the message is off the session regardless — its traffic is dropped, not
merely discouraged.

Kicking is not banning. Without a stable identity a kick lasts exactly until the
offender reconnects.

### Identity

A signed-in client presents its account claim in the join handshake — subject
id, display name, tier. The server records it and `net.identity(peer)` reports
it:

```lua
local who = net.identity(peer)   -- { id, name, tier, verified }
```

`id` is the account's stable subject: the same across sessions and machines,
which is what lets a returning player be recognised, a ban outlive a reconnect,
and a statistic be attributed to somebody.

**Anonymous play is a normal state.** A LAN or friends game with nobody signed
in works exactly as it always has; such a peer has no `id` and `verified` is
`false`. A server that wants an account says so:

```lua
net.host{ requireIdentity = true,
          deny  = { "user_griefer" },     -- refused at the door
          allow = { "user_a", "user_b" }} -- if non-empty, invite-only
```

All three are consulted **before** the join is accepted, which is the difference
between a ban and a chore. Refusals carry a reason the client can show. Every
refusal and kick is printed by `floptle serve`, because a dedicated server has
nobody watching a Console and a moderation action nobody can audit is not a
moderation tool.

### What `verified` means, and why it is false

> **Today `net.identity(peer).verified` is `false` for every peer.** The engine
> carries what a client *says* about itself. Turning that into an identity means
> checking it with the provider, and doing that needs a credential scoped to the
> server you are joining — a full-scope access token would let any server you
> join spend your Fobucks and read your mail, so the engine deliberately does
> not send one. `contracts/identity-auth.md` has no such credential yet; it is
> filed as engine task `0184`.

So: allow lists, deny lists and `requireIdentity` all work, and what they buy
today is keeping out the careless rather than the determined. The engine reports
its own confidence honestly instead of dressing an assertion up as proof,
because a server operator *acts* on that flag — and a moderation tool that lies
about how sure it is, is worse than none. When the credential lands, the same
code starts seeing `verified = true` and nothing else changes.

---

## 7. Rehearsing a bad connection

The simulated-link sliders only shape the in-editor harness. To put real latency
on a **real** QUIC or relay session between two instances on one desk, start the
editor with:

```
FLOPTLE_NET_IMPAIR= cargo run -p floptle-editor            # on, sliders at zero
FLOPTLE_NET_IMPAIR=50ms,2% cargo run -p floptle-editor     # on, pre-dialled
```

The value is one-way latency and loss (`"50ms,2%"`, `"50"`, `"2%"`, or empty).
50 ms one-way is ≈100 ms RTT — the middle of the band a fighting game has to
feel right at.

A **⚠ LINK IMPAIRMENT (dev build)** section then appears in the 🌐 panel with
one-way latency and loss sliders you can move live. The section does not exist
at all without the environment variable, so a real session can never be silently
degraded from the UI.

It is not a network emulator — no jitter, no reordering — and it is not a
substitute for a real two-machine run. Reliable traffic is never dropped, because
a real reliable channel retransmits and dropping handshakes would only invent
failures the field can't produce.

---

## 8. Reference

| Where | What |
|---|---|
| [scripting.md §16](scripting.md) | `net.*`, `synced`, `onRpc`, `net.rewind` — the full API |
| [scripting.md §16b](scripting.md) | `snapshot`/`restore`, `net.random`, the rollback rules |
| §6b above | `net.kick`, `net.identity`, allow/deny — running a server anyone can reach |
| [scripting.md §16c](scripting.md) | `voice.*` — proximity voice chat, and why range gating is the server's job |
| ADR-0022 | why open netcode with a self-hostable relay |
| ADR-0025 | why rollback, and what it costs |
| [export-builds.md](export-builds.md) | shipping a build that hosts and joins |
