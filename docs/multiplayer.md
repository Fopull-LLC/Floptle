# Making a multiplayer game

This is the **build-something** guide. It goes from a single-player scene to two
machines playing together, then to shipping. For *why* any of it works the way it
does, see [netcode-design.md](netcode-design.md) and
[rollback-netcode-design.md](rollback-netcode-design.md); for the full API
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

### Testing a rollback match

Local first: **⏵ Host + join a local client** with the latency slider at 6–8
ticks. Then rehearse at real conditions on one desk — see §7.

The 🌐 panel is your instrument:

- **corrections / depth last / max / avg** — a healthy match sits at *low average
  depth*. `max` is the worst moment; `avg` is the texture of the connection.
- **guessed N% of ticks** — the mispredict rate. Rises with latency; it's what
  you choose the input delay against.
- **🥊 ROLLBACK · waiting for input** (orange) — a stall. Past the depth cap the
  sim waits rather than guessing further, so the game runs slightly slow instead
  of teleporting the opponent. It recovers on its own. Show your own "connection
  trouble" banner off `net.stalled()`, because otherwise a stall is
  indistinguishable from a bad frame rate.
- **✔ checksums agree through tick N** — the thing you want to see. Checksums
  are mandatory and always on: every 30 confirmed ticks each peer hashes its
  state and the host compares.

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
| [netcode-design.md](netcode-design.md) | the design: replication, prediction, interest, lag comp |
| [rollback-netcode-design.md](rollback-netcode-design.md) | the rollback design: input delay, the ring, checksums, the referee |
| [rollback-p7-fofighter-checklist.md](rollback-p7-fofighter-checklist.md) | the two-machine acceptance run |
| [export-builds.md](export-builds.md) | shipping a build that hosts and joins |
