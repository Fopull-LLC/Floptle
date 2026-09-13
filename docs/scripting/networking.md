# Multiplayer

Replication, remote calls, rollback, and voice.

Part of the [scripting guide](../scripting.md) · [every call, as a reference](../lua-api.md)

## Contents

- [16. Networking: `net.*`, `synced`, `onRpc`](#16-networking-net-synced-onrpc)
- [16b. Rollback netcode: `snapshot`, `restore` & `net.random`](#16b-rollback-netcode-snapshot-restore-netrandom)
- [16c. Voice: proximity chat](#16c-voice-proximity-chat)

---

## 16. Networking: `net.*`, `synced`, `onRpc`

Multiplayer in Floptle is **server-authoritative**: the host simulates the
truth, clients receive smoothed snapshots, and clients send *intents* (RPCs),
never state — so cheating means asking the server nicely. Making a node
multiplayer takes two steps, no rewrite:

1. Give it the **Networked** component (Inspector → ➕ Add Component →
   Networking), or from code: mark what syncs in its settings.
2. Declare which script vars sync with a top-level `replicated` table, and
   read/write them through `synced`:

```lua
-- door.lua — a fully networked, late-joiner-correct door in ten lines
replicated = { open = false }

onRpc = {}
function onRpc.use(args, sender)          -- a client walked up and sent net.rpc("use")
  if net.isServer() then synced.open = not synced.open end
end

function update(node, dt)                 -- cosmetic: everyone eases toward the truth
  local target = synced.open and 1.6 or 0.0
  node.y = node.y + (target - node.y) * math.min(1, dt * 6)
end
```

| Call | What it does |
|---|---|
| `net.host{ maxPlayers = 16, port = 7777, relay = "addr" }` | become the authoritative host — `relay` = get a LOBBY CODE through a rendezvous relay (nobody port-forwards); `port` = direct UDP (QUIC); neither = the in-editor harness |
| `net.host{ interest = 150, interestBudget = 16384 }` | **interest management** — each client is told about its own neighbourhood (metres) within a per-client byte budget, instead of everything. Absent = broadcast to everyone, which is cheaper below a few dozen players. Tick ⬦ *always relevant* on a node's Networked component to exempt it (the match clock, the objective, the boss) |
| `net.lobbyCode()` | the five letters friends type in, on a relay host — so your own lobby screen can show them. **Poll it**: `nil` until the relay answers (a round trip after `net.host`), and `nil` for good on a client or a direct/LAN host, where there is no code and joiners use the address |
| `net.join(addr)` | join a session (`"relay://relayaddr/CODE"` = by lobby code; `"quic://host:port"` = a server directly; `"local://"` = the in-editor test harness). **Does not block** — see `net.joinState()` |
| `net.joinState()` | `"offline"` / `"connecting"` / `"joined"` / `"refused"` / `"starting"`, plus the reason as a second return when refused. `"starting"` means the lobby is real and its dedicated server is waking up — not a refusal, and unlike `"connecting"` it can last tens of seconds, so show the second return rather than a spinner. **Wait on this, not on `net.role()`** — joining doesn't block, so role reads `"client"` from the frame you called `net.join`, whether or not that code matched any lobby |
| `net.leave()` | end the session |
| `net.role()` / `net.isServer()` / `net.isClient()` | `"offline" \| "server" \| "client"` |
| `net.peers()` / `net.ping(peer)` | connected peer ids · round-trip ms |
| `net.rpc(name, args, {to=peer, withInput=true})` | remote call — server→clients, or client→server; `withInput` stamps the tick you were seeing (for `net.rewind`) |
| `net.on(event, fn)` | `"playerJoined"/"playerLeft"` (peer id — `playerLeft` also gets the kick reason, when there was one), `"connected"`, `"disconnected"`, `"kicked"` (why the server removed you) |
| `net.identity(peer)` | `{ id, name, tier, verified }` — who that peer is. **Check `verified`**: it is `false` for everyone today (see below) |
| `net.kick(peer, reason)` | SERVER: remove a player, with words that reach them |
| `net.host{ requireIdentity = true, allow = {ids}, deny = {ids} }` | who this server will admit, consulted **before** a join is accepted |
| `net.spawn(path, {x,y,z,owner})` | SERVER: spawn a scene or prefab's first root **and everything under it**, replicated everywhere |
| `net.despawn(node)` | SERVER: remove it — and its subtree — everywhere |
| `net.setOwner(node, peer)` | SERVER: hand a replicated node to `peer`, or `nil` to release it. What gives a reconnecting player their slot back |
| `net.host{ interestOcclusion = "Level" }` | also require **line of sight**, tested against that collision layer. On top of `interest`, never instead of it |
| `net.setRelevant(node, peer, bool)` | SERVER: decide per client whether that client is told about that node at all (`nil` = let the radius and the sight test decide). The hidden-role hook |
| `net.rewind(peer, fn)` | SERVER: run `fn` against the world as `peer` perceived it (lag compensation) |
| `net.isMine(node)` | is this node under MY control here? (cameras/HUDs pick the local player; pair with `findScripts`) |

**Who a peer is, and getting rid of one.** A session peer used to be a
transport id plus whatever name the game's own handshake carried, and there was
no `kick` at all. Now a signed-in client presents its account claim when it
joins, the server records it, and `net.identity(peer)` reports it:

```lua
net.on("playerJoined", function(peer)
  local who = net.identity(peer)
  if who.verified and bans[who.id] then
    net.kick(peer, "you are banned from this server")
  end
end)

net.on("kicked", function(_, reason)         -- on the removed player's machine
  ui.show("You were removed: " .. reason)
end)
```

> **`verified` is `false` for everyone right now, and that is not a bug.** The
> engine carries what a client *says* about itself; turning that into an
> identity needs a credential the server can check with fopull.com, scoped so
> that presenting it does not also hand that server your account. No such
> credential exists yet (it is filed as engine task `0184`). So an allow/deny
> list and `requireIdentity` work, and they keep out the careless rather than
> the determined — and `net.identity` says so rather than reporting a
> confidence it does not have. Do not key a ban list or a statistic on an
> unverified `id` and call it settled.

Anonymous play is untouched: a LAN or friends game with nobody signed in joins
exactly as it always did, and `verified = false` with no `id` is a normal state.
`net.host{ requireIdentity = true }` refuses an anonymous join with a reason the
client can show.

**A radius is not a security boundary.** Interest management exists to save
bandwidth, and for an open-world game that is the whole feature. In a
hidden-role or competitive game, seeing another player through a wall *is* the
game — and a client that has been told where everyone within the radius is
standing knows where they are, whatever it chooses to draw. Nothing you do on
the client side fixes that: hiding, attenuating or not rendering something the
client already received is a setting a modified client turns back off.

Two server-side answers, and they compose:

```lua
-- Line of sight, on top of the radius. Nothing on the "Level" layer between
-- you and it, or you are not told about it. Off unless you ask.
net.host{ interest = 25, interestOcclusion = "Level" }

-- Your own rule, per (client, node) — whatever the geometry says.
net.setRelevant(killerNode, survivorPeer, false)   -- withhold
net.setRelevant(objective, peer, true)             -- pin, at any distance
net.setRelevant(killerNode, survivorPeer, nil)     -- let the tests decide again
```

Losing sight is damped by a few snapshots so a body behind a door frame does not
flicker; regaining it is immediate, because a player stepping out of cover has
to be there on the frame they step out. A client is **always** told about its
own avatar and about anything flagged *always relevant* — a filter cannot hide
either, since the first is what prediction reconciles against. Losing relevance
never despawns a scene-authored node; it stops being updated and is sent in full
on re-entry. The 🌐 panel shows, per client, how many nodes are being withheld
and by which rule.

For a **fighting game** — where the whole game is reading your opponent's exact
state this frame — the mode above is the wrong shape. See
[§16b, rollback netcode](#16b-rollback-netcode-snapshot-restore--netrandom).

**`synced` rules.** Values can be numbers, booleans, strings, and tables
(nested up to 4 levels, ≤ 1 KB encoded per var — an oversized write is dropped
whole with a Console warning, never truncated). Only the **server's** writes
replicate; writing on a client warns and gets overwritten. Late joiners receive
the current values automatically.

**RPC handlers** live in an `onRpc` table: `function onRpc.use(args, sender)`.
`sender` is the *verified* peer id (`0` = the server) — clients can't spoof it.
Args follow the same size/type rules as `synced`.

> **Test it without a second machine:** press Play, then the **🌐** toolbar
> button → *Host + join a local client*. A hidden ghost client joins over a
> simulated link — **cyan ghost spheres** show where *it* believes every
> networked node is. Drag the latency/loss sliders and watch the ghosts lag
> and stutter exactly as a real remote player would.

> **Play over a real network:** both machines open THIS project and press
> Play. One hosts (🌐 → *Host on LAN*, or `net.host{ port = 7777 }`), the
> others join (`quic://<host's-LAN-ip>:7777`). The link is QUIC — encrypted,
> zero-config (the trust model of a Minecraft server; verified identity comes
> with the relay). Player slots: **scene-authored Predicted nodes, in node
> order — #1 is the HOST's, #2 the first joiner's, #3 the second's**, and so
> on. Duplicate your character node to add a slot, and every camera/HUD picks
> its own player via `net.isMine` (the stock camera already does).

### Per-player avatars: spawn one on join

The scalable shape — no authored slot per player. The server spawns an avatar
scene for each joiner; the engine registers its physics body live, the
joiner's machine binds **prediction** to it (instant response at any latency),
everyone else interpolates it, and it despawns automatically when its player
disconnects:

```lua
-- player_spawner.lua — attach to any always-present node (the Map)
function start(node)
  net.on("playerJoined", function(peer)
    if net.isServer() then
      net.spawn("scenes/player.ron", { x = peer * 2, y = 2.5, z = 8, owner = peer })
    end
  end)
end
```

`scenes/player.ron` can be a whole rig: a capsule with a RigidBody, your
controller scripts, a Networked component set to *Predicted*, and whatever
hangs off it — a camera child, a first-person arms mesh, a point light, a
bone-attached item socket. The first root and its entire subtree spawn on the
server and on every client, parent links, bone attachments and scripts intact.
The scene's own Predicted node (if any) stays the host's avatar.

**Only the root replicates.** Its children are ordinary local nodes that follow
it, and their scripts run on every peer — the same deal an authored child of a
networked node has always had. Give a child its own Networked component and it
replicates in its own right, under the same owner (a creature's model child
syncing its animator, say). Spawns are dynamic bodies, not static geometry.

**Ownership is not fixed at spawn.** `net.setOwner(node, peer)` hands a
replicated node to a player after it exists, and `net.setOwner(node, nil)`
releases it — which is how a player who dropped gets their own slot back on
reconnect rather than a fresh one:

```lua
net.on("playerLeft", function(peer)
  local slot = mySlots[peer]
  if slot then net.setOwner(slot, nil) end   -- free it, keep the body
end)
```

**On a dedicated server**, authored `Predicted` slots are handed out from #1 in
node order as players join, and freed when they leave. That differs from a
hosted session on purpose: there, slot #1 belongs to the host, because somebody
is sitting at that keyboard. A dedicated server has nobody, so reserving #1
would leave an avatar in the world that no client predicts and no input drives.
A slot your own script assigns is never reassigned behind your back.

### Lobby codes: play without port-forwarding

Run the open relay anywhere both machines can reach (`floptle-relay`, one
binary, default port 7788 — or use a managed one), then:

- **Host:** 🌐 → *Host via relay* (or `net.host{ relay = "relay.host:7788" }`)
  → you get a five-letter **lobby code**.
- **Friends:** 🌐 → Join with `relay://relay.host:7788/CODE`
  (or `net.join("relay://…/CODE")`).

Show the code on your own lobby screen rather than sending players to the 🌐
panel — `net.lobbyCode()` returns it:

```lua
function update(node, dt)
  find("CodeLabel").text = net.lobbyCode() or "getting a code…"
end
```

Poll it rather than reading it once: the relay has to answer first, so it is
`nil` for a round trip after `net.host`. It stays `nil` on a client and on a
direct/LAN host — there is no code in either case — and it clears the moment a
session ends or a host attempt fails, so five stale letters can never sit on
screen looking live.

**Joining does not block, and the code is usually wrong.** `net.join` returns
immediately and `net.role()` reads `"client"` from that frame — before the relay
has said anything. A game that trusts role congratulates a player on joining a
lobby that was never there. Wait on `net.joinState()` instead:

```lua
function update(node, dt)
  local state, why = net.joinState()
  if state == "joined" then
    scene.load("arena")
  elseif state == "refused" then
    find("Error").text = why          -- "no lobby QK7RM", in the relay's words
  end
end
```

Mistyping the code is the most common thing that will ever go wrong in an online
session. `"refused"` is the relay actively saying no; a relay that is switched
off entirely never answers at all, and stays `"connecting"` — so give that case a
timeout of your own.

The relay is dumb on purpose: lobbies, peer ids, forwarding — it never reads
game state, and a session through it is byte-identical to a direct one. The
lobby dies when its host leaves. Self-host it forever, no strings — the
managed convenience (always-on relays near your players) is what Floptle
Cloud sells.

**Prediction** (*🌐 → Test as remote player*): give your character's node a
Networked component with mode **Predicted (owner)** and it responds instantly
at any latency — the engine records your inputs, the server re-runs the same
script with them, and divergences rewind-replay invisibly. One thing to know:
**in a session, a predicted node's `update` runs on the gameplay tick** (60 Hz,
constant `dt`) instead of per frame, so the client and server integrate your
controller identically. Your script doesn't change — but movement code belongs
in `fixedUpdate` anyway, and cameras (per-frame `update`) belong on a separate,
non-networked node.

**Which scripts run where.** On a client, a node whose **transform/physics**
the server owns is fully snapshot-driven — its scripts don't run there (its
state arrives over the wire). A Networked node that only syncs script **vars**
runs its scripts everywhere: that's the door above — `update` eases toward
`synced.open` on every machine, and the authoritative flip guards with
`net.isServer()`. Rule of thumb: sync the transform for things physics moves;
sync only vars for things scripts animate.

### Lag-compensated combat: `withInput` + `net.rewind`

On your screen, every *other* player is rendered a beat in the past (the
interpolation delay) — so by the time your "I swung" intent reaches the server,
the defender has moved on. Judged at server time, hits you clearly landed
whiff, and parries that were up on your screen don't count. The fix is the
genre's standard contract: **the server rewinds the world to what you saw and
judges there.**

Two pieces. The client stamps the intent with the tick it was seeing; the
server wraps its hit-check in `net.rewind`:

```lua
-- sword.lua — on the attacker (a Predicted node)
function update(node, dt)
  if net.isClient() and input.clicked(0) then
    local yaw = input.aimYaw() or node.yaw
    net.rpc("swing", { dx = math.sin(yaw), dz = math.cos(yaw) },
            { withInput = true })                 -- ← stamp what I was seeing
  end
end

onRpc = {}
function onRpc.swing(args, peer)                  -- runs on the SERVER
  net.rewind(peer, function()                     -- ← the world as PEER saw it
    local hit = raycast(node.x, node.y, node.z, args.dx, 0, args.dz, 3.0)
    if hit and hit.node then
      local combat = hit.node:getscript("combat")
      if combat and combat.synced.parrying then   -- their flag AT THAT TICK
        net.rpc("parried", { by = hit.node.id }, { to = peer })
      elseif combat then
        combat.hurt(25, peer)
      end
    end
  end)
end
```

Inside the `net.rewind` closure, **raycasts and shape queries see every
networked body where that player saw it**, and **other scripts' `synced` vars read the values from
that same tick** — so a parry window that was open on the attacker's screen
counts, even if it just closed at server time. Everything snaps back to the
present when the closure returns (it also passes through return values, so
`local hit = net.rewind(peer, function() return raycast(...) end)` works).

The fine print, so you can reason about fairness:

- `raycast` hits **physics bodies** (players, crates) as well as static
  geometry, and tells you who: `hit.node` is the node's handle either way.
  Your own body is always excluded from your rays, and an optional trailing
  arg skips one more node: `raycast(…, max, someNode)` — what the orbit
  camera does so the character it follows never reads as a wall.
- Rewind depth is **clamped to ~250 ms** — a very-high-ping attacker can't
  shoot everyone else in the distant past. Beyond the clamp, their disadvantage
  is real (that's the honest tradeoff every game in the genre makes).
- `net.rewind` outside a server-side `onRpc` handler for a `withInput` rpc
  (or with the wrong peer) warns and runs the closure at server time — your
  logic still works, it's just not compensated.

---

## 16b. Rollback netcode: `snapshot`, `restore` & `net.random`

Everything in §16 is *server-authoritative*: one machine simulates, the others
watch a slightly delayed copy and predict their own avatar. That is the right
shape for almost every game — and the wrong shape for a fighting game, where
the decision you make is made against your opponent's exact state *this frame*.

**Rollback** is the third mode. Set a node's Networked component to
`Rollback (every peer)` and every machine simulates that node, every tick, from
the session's per-tick inputs. Nothing about a hit ever crosses the wire — only
inputs do — so hit resolution, hitstop and meter agree because the *simulation*
agrees. Why it works this way:
the rollback decision record (ADR-0025).

### The contract: two hooks

```lua
function snapshot()   return { hp = hp, meter = meter, frame = frame } end
function restore(s)   hp, meter, frame = s.hp, s.meter, s.frame end
```

That is the whole opt-in. When a remote input arrives that contradicts what was
predicted, the engine restores the last agreed tick and re-simulates every tick
since, with no rendering in between — and it restores your script's state
through these two hooks.

> **A script that defines neither hook is not rolled back — right for
> cosmetics, wrong for gameplay.**

Read that twice, because nothing warns you at runtime: a rolled-back match with
one un-snapshotted counter keeps playing, and the two machines simply stop
agreeing about it. If a value affects what happens, it belongs in `snapshot()`.

The engine owns the copy in both directions, so a replay can't corrupt the
snapshot it restored from. Rollback state holds numbers, strings, booleans and
nested tables; a node handle or a function is refused with a Console error,
because those cannot be meaningfully restored and silently dropping them would
produce a state that *looks* restored and isn't.

### Writing a fighter that stays in sync

| Rule | Why |
|---|---|
| Count **frames**, not seconds | `heldSecs` reads 0 on rollback-driven slots (the wire carries actions, not durations). Integer frame counts are exact and re-simulate identically. |
| Build hurtboxes from `node.tickPos`, never `node.x` | Between ticks the transform holds the *interpolated render pose*. Reading it in `fixedUpdate` is a frame-rate-dependent read that no replay can reproduce. |
| Move the body with `node.tickX/tickY/tickZ` or velocity, never `node.x = node.x + d` | Same reason, from the other side: that teleports the body onto its **visual** position — the model slides and the hurtbox doesn't. |
| Use `net.random()`, never an unseeded `rng()` | An unseeded roll comes from the clock. Two peers draw different numbers and the match quietly forks in two. |
| Put projectiles in rollback **state**, not in `spawn()` | A spawned prefab isn't part of the rollback state and one-shot spawns are suppressed during a replay. A fireball that must exist on both machines is data in your controller's snapshot, rendered by the controller. |
| Turn on **pushbox only** on the RigidBody | The contact solver is the part least likely to agree bit-for-bit between two machines. With it on, the body integrates its velocity and nothing else — your script owns gravity, the floor and pushout, which is how the genre works anyway. |

### The API

| Call | What it does |
|---|---|
| `net.random()` / `net.random(n)` / `net.random(a, b)` | deterministic RNG: `[0,1)`, `1..n`, `a..b`. Drawn from (match seed, tick, draw index), so every peer rolls the same numbers **and a re-simulated tick rolls them again** |
| `net.replaying()` | true while the engine is re-simulating. For cosmetics it can't see (a material poke, a UI label) — **never** branch simulation on it, that IS a desync |
| `net.rollbackDepth()` / `net.rollbackMax()` / `net.rollbackAverage()` | ticks re-simulated by the last correction · the worst so far · the mean per correction |
| `net.mispredictRate()` | 0..1 — the fraction of ticks that had to guess |
| `net.inputDelay()` | the session's fixed input delay, in ticks |
| `net.stalled()` | the sim is waiting for input rather than guessing further — show your own "connection trouble" banner off this |
| `net.on("desync", fn)` | the peers' checksums disagreed. From here the two machines are playing different matches; end the set honestly rather than play it out |

The engine suppresses one-shot side effects during a re-simulation —
`spawnEffect`, `audio.play`, prefab `spawn()`/`destroy()`, `net.rpc`, console
output. The honest consequence: a correction can *eat* a cosmetic (the spark
that only exists on the corrected timeline never fires) or *orphan* one (the
spark fired for a hit that turned out not to happen). Every rollback game lives
with this; at the depth cap it reads as network crackle, not wrongness.

### What the engine does not promise

- **Same build, same platform:** determinism is guaranteed for the profile
  above.
- **Across platforms** (x64 ↔ Apple Silicon): expected, not proven. IEEE
  add/mul/div/sqrt are bit-exact everywhere; the risk is `sin`/`cos`/`pow`/
  `atan2`, which may differ in the last bit between platform math libraries. A
  fighter's simulation path typically avoids them.
- **Bodies in solver-resolved contact:** out of scope — that is what
  `pushboxOnly` is for.

Which is exactly why checksums are **mandatory and always on**: every 30
confirmed ticks each peer hashes its state and the host compares. A mismatch is
loud — Console error naming the tick, red in the 🌐 panel, and
`net.on("desync")`. A rollback implementation without them doesn't fail
loudly; it plays a subtly different match on each screen until someone notices
the health bars disagree.

### Frame-stepping backwards

While paused, **⏮ Back (Shift+F3)** puts the simulation back one gameplay tick.
A simulation isn't invertible, so this reads the rollback state ring rather than
re-deriving anything: it needs a rollback session running and reaches back about
a fifth of a second. Stepping back and forward again lands on exactly the state
that was there — it's a scrubber, not an undo.

---

## 16c. Voice: proximity chat

A remote player's voice is an **ordinary spatial sound**. It is attenuated by
distance, panned by direction, and routed through a mixer track like every other
sound in the game — which is what makes the rest of this short.

| Call | What it does |
|---|---|
| `voice.devices()` / `voice.device()` | input device names · the one that's open. Empty list = no microphone, which is normal |
| `voice.setDevice(name)` | open it (`nil` = the system default) |
| `voice.setTransmit(on)` | open/close the mic — **push-to-talk is your call**, this is where it lands |
| `voice.level()` | mic level 0..1 for a settings meter. Live whether or not you're transmitting |
| `voice.sidetone(on)` | hear yourself. Off by default |
| `voice.attach(peer, node, opts)` | that player's voice comes out of that node and follows it |
| `voice.source(peer)` | a handle like `audio.play` returns — `:setTrack`, `:setVolume`, `:setPosition(node)`… |
| `voice.speaking(peer)` / `voice.mute(peer, on)` | HUD indicator · a **local** mute |
| `voice.setForward(peer, {peers})` | **SERVER**: who may hear that speaker (`nil` = everyone) |

### The whole thing, in a game

```lua
function start(node)
  voice.setDevice(nil)                       -- the system default mic

  net.on("playerJoined", function(peer)
    voice.attach(peer, avatarFor(peer), {
      mode = "Spatial", falloff = "Inverse",
      minDistance = 2, maxDistance = 22,
      track = "Voice",
    })
  end)
end

function update(node)
  voice.setTransmit(input.action("Talk"))    -- push-to-talk
end
```

### Making a monster out of a voice

There is no per-effect voice API, and there does not need to be one. Author the
tracks in `project.ron` — `Voice` (clean), `Voice Monster` (PitchShift −6 st,
Distortion, Chorus, a low shelf, Reverb), `Voice Dead` — and move a speaker
between them:

```lua
voice.source(peer):setTrack(isMonster and "Voice Monster" or "Voice")
```

### Who can hear you is the server's decision

This is the part worth reading twice.

```lua
-- on the server, every tick or so
for _, speaker in ipairs(net.peers()) do
  voice.setForward(speaker, whoIsWithinEarshotOf(speaker))
end

voice.setForward(deadPeer, deadPeers)   -- the dead talk to the dead
```

A peer that is not on that list **is never sent the audio at all**. That matters
more than it sounds: turning a stream down on the receiving client is a volume
slider a modified client turns back up, and in a game where hearing someone is
knowing where they are, that is the difference between a mode that can ship
competitively and one that cannot. Range gating belongs on the server, and
`voice.setForward` is where it goes.

### What it does when things go wrong

- **No microphone.** `voice.devices()` is empty, every other call is a no-op,
  and the game runs. Most machines are this machine.
- **A lost packet** is a gap Opus conceals — never a stall, and never the end of
  the stream. A voice that ended could not resume, and the player would go
  silent mid-sentence for the rest of the match.
- **A late packet** widens the jitter buffer, up to 60 ms; a link that settles
  earns the latency back. The 🌐 panel shows the cushion per speaker.
- **`scene.load`** does not cut anyone off. The capture and the streams live
  with the *session*, not the scene — re-attach in the new scene and the stream
  never restarted.
- **A dedicated server** captures nothing and plays nothing. It forwards, which
  is all it should do with no audio device and no listener.

### Trying it without a second machine

Voice normally needs two machines, two people and a microphone to test at all,
which is what makes it the feature most likely to ship broken. The 🌐 panel has
**🎤 test voice from a WAV…**: it plays an audio file in as though a remote
player were speaking it, through the real forwarding rules, the real jitter
buffer and the real spatial voice. If it comes out of the right avatar at the
right volume, the routing is right.
