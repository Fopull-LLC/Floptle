# Floptle Netcode — Phase 2 design (`floptle-net`)

- **Status:** Draft for review · 2026-07-06
- **Author:** Ty Johnston (Fopull LLC)
- **Companion:** [ADR-0022](decisions/0022-networking-and-cloud.md) ·
  [networking proposal](networking-proposal.md) (the business/topology shape) ·
  [`subsystems/networking-future.md`](subsystems/networking-future.md) (the original foundation
  sketch — this doc supersedes its §3–§4 sketches with a committed design)
- **Scope:** Phase 2 of ADR-0022 — the **open engine netcode MVP**. Floptle Cloud (Phase 3)
  and dedicated hosting (Phase 4) build on this but are out of scope here.

## 1. The flagship scenario, and what it demands

The design target is a **parry-based melee MMO** in the Deepwoken lineage: timing-window combat
(parries land or fail inside ~100–300 ms), dozens-to-hundreds of players per instance, and a
server that cannot trust clients. That target is deliberately the *hardest* common case — if the
netcode carries it, co-op platformers and racing games are easy.

It decomposes into four hard requirements:

1. **Client prediction + server reconciliation** — your own character must respond *this frame*,
   with the server silently re-validating. Anything else feels like syrup.
2. **Lag compensation** — a parry must be judged against **what the player saw**, not what the
   server's clock says 80 ms later. The server rewinds combat queries to the acting client's
   perceived time. This one feature decides whether parry gameplay is *fair*; it is designed in
   from day one, not bolted on.
3. **Interest management** — beyond ~30 players, replicate-everything melts bandwidth and CPU.
   Each client receives only what it can perceive, prioritized by relevance.
4. **Server authority by construction** — clients send **inputs and intents, never state**.
   Cheating becomes "asking the server nicely."

And one product requirement that outranks all of them:

5. **Ease of use is the product.** Making a game multiplayer must not mean rewriting it. The
   developer writes *one* character controller, marks what replicates, and the engine does
   prediction, reconciliation, interpolation, interest, and lag compensation. The API below is
   designed backwards from "what would be genuinely enjoyable to type."

## 2. What exists, and the gaps this design closes

An architecture survey (2026-07-06) confirmed the engine leans the right way, with four concrete
gaps:

| Have | Gap |
|---|---|
| Fixed-step physics `Sim` at 120 Hz with render interpolation (`sim.rs`) | **Scripts + input run per rendered frame at variable dt** — prediction needs gameplay on the fixed tick |
| Generational `Entity` handles | **Not stable across sessions/machines** — need a real `NetId` |
| RON `*Doc` serialization layer for all authored state | **Live physics state (`Body.pos/vel`) is Sim-local, non-serializable** — snapshots need a wire form |
| Floating origin (origin-relative f32 body coords) | **Wire protocol must speak absolute f64**; each peer rebases locally |
| `floptle-net` boundary stub, gameplay never touches sockets | Everything else in this doc |

Determinism status: physics is f32 semi-implicit Euler — deterministic for identical inputs on
the same build/machine, **not** bit-identical cross-platform. That rules out lockstep and is
*fine* for this design: server-authoritative reconciliation only needs *approximate* client
re-simulation, because every snapshot re-anchors the client to authoritative truth. Prediction
error shows up as an occasional sub-centimetre smoothed correction, not desync.

## 3. Foundations rework: the gameplay tick

The single prerequisite change, valuable even without networking:

- **A gameplay fixed tick** (default **60 Hz**, configurable per project) becomes the unit of
  simulation. Physics keeps its 120 Hz substep (2 per tick). The render loop interpolates,
  exactly as it already does for physics.
- **New Lua lifecycle hook: `fixedUpdate(node, dt)`** — runs once per gameplay tick with
  constant `dt`. `update(node, dt)` stays (per-frame, for cameras/visuals). Movement/gameplay
  code migrates to `fixedUpdate`; docs teach the split as "gameplay in `fixedUpdate`,
  cosmetics in `update`."
- **Input is snapshotted per tick**, not per frame: edge states (`pressed`/`released`)
  accumulate between ticks so nothing is lost, and the per-tick `InputSnapshot` is exactly what
  gets recorded, numbered, and sent to the server for predicted nodes (§6).
- **Ordering audit**: consumers of `HashMap`/`HashSet` state in the script host
  (`set_bodies`, key sets) get deterministic iteration (sort or `Vec`) where order can leak into
  sim results.

Ticks are numbered (`u64` from sim start); the tick number is the timebase for snapshots, input
commands, and the lag-compensation history.

## 4. Identity, authority, replication model

### 4.1 `NetId`

A `NetId(u64)` allocated by the server, stable for the life of a networked object, mapped
bidirectionally to `Entity` per peer. Scene-authored networked nodes get deterministic ids
(hash of scene path + node path) so a level's static networked set needs no spawn messages;
runtime spawns allocate from a server counter.

### 4.2 The `Replicated` component

```rust
struct Replicated {
    id:        NetId,
    mode:      ReplicationMode,   // how clients treat it
    owner:     Option<PeerId>,    // whose inputs drive it (predicted mode)
    transform: bool,              // sync position/rotation (+ interpolate on remotes)
    physics:   bool,              // sync velocity too (better extrapolation/prediction)
    interp:    bool,              // smooth remote entities between snapshots (default on)
}

enum ReplicationMode {
    Authority,   // server simulates; clients render interpolated snapshots (default)
    Predicted,   // owner-client ALSO simulates locally, ahead of the server (its own avatar)
}
```

Editor-first: **"Networked" is an Add-Component in the Inspector** like RigidBody/Material —
checkboxes for transform/physics/interp, a mode dropdown. The zero-code path to a moving
synced object is: add Networked, press Play as host, join, watch it move.

Authority is **binary and server-rooted**: the server owns everything; `Predicted` is not
client authority, it's client *optimism* — the owner simulates ahead and the server's word
remains final (§6). There is deliberately no `Authority::Client` free-for-all mode in v1; that's
how cheating happens.

### 4.3 Replicated script variables

Mirrors the existing `defaults` → `params` idiom exactly:

```lua
defaults   = { walk = 4.5, run = 8.0 }          -- existing: inspector tunables
replicated = { health = 100, stamina = 100, parrying = false }  -- NEW: synced vars
```

At runtime scripts read/write **`synced.health`** the way they read `params.walk`. Writes are
authoritative only on the server (client writes to a non-predicted var log a warning in the
Console); deltas ride the snapshot stream. v1 value types: numbers, booleans, strings, **and
tables** (nested scalars, depth ≤ 4, ≤ 1 KB encoded per var — guardrails in §13.2). `replicated`
on a `Predicted` node's script participates in rollback (§6): predicted writes are speculative
until acknowledged.

## 5. The wire model

### 5.1 Messages

```
client ──▶ InputCommand   { tick, seq, input_snapshot, intents[] }        every tick (unreliable-seq)
server ──▶ Snapshot       { tick, ack_seq, baseline_tick, delta[] }       at snapshot rate (unreliable-seq)
server ──▶ ReliableEvent  { spawns, despawns, rpc, joins/leaves }         reliable-ordered
client ──▶ Rpc            { name, args }                                  reliable-ordered
```

- **Snapshots are delta-compressed** against the last client-acked baseline per peer (classic
  Quake 3 model — survives loss without resend round-trips: lose a snapshot, the next delta is
  just computed from an older acked baseline).
- **Snapshot rate** decouples from tick rate: sim at 60 Hz, send at **30 Hz default**
  (configurable 10–60). Remote entities interpolate ~100 ms behind; the local predicted avatar
  runs ahead. Bandwidth scales with snapshot rate × interest set, not tick rate.
- **Quantization:** wire transforms are absolute-f64 position (floating-origin-safe, §2),
  quantized where safe (rotation as smallest-three, velocities f16-ish) — exact packing decided
  at implementation with a bandwidth test, not guessed here.
- `intents[]` are the client→server RPCs that piggyback the input stream (e.g. `swing`,
  `parry`) so a combat action and the movement of the same tick arrive together and are judged
  against the same rewound state (§7).

### 5.2 Interest management (the player-count feature)

Per client, per snapshot, the server builds the **relevant set**:

- **Spatial**: a coarse grid over networked entities; everything within `interest` radius of
  the client's avatar (default 150 m, per-host config) is a candidate. Always-relevant flags
  exist for global objects (match state).
- **Priority accumulator** (Source/Overwatch style): each candidate accrues priority per
  snapshot from (changed-recently, proximity, is-a-player, owner-visibility); each snapshot
  spends a **per-client byte budget** (default ~16 KB/s) on the highest-priority entities;
  unsent entities keep accruing. Nothing is ever *dropped*, only deferred — a far-away idle
  crate syncs eventually, a nearby fighting player syncs every time.
- Entities leaving the relevant set despawn on that client (with a grace hysteresis band so
  boundary-hovering doesn't flicker); re-entering re-spawns from full state.

This is what raises the per-instance ceiling honestly: bandwidth per client stays flat as the
*world's* population grows, because each client only pays for its neighbourhood.

### 5.3 Transport

The trait from `networking-future.md` §4, unchanged in spirit:

```rust
pub trait Transport: Send {
    fn send(&mut self, peer: PeerId, channel: Channel, bytes: &[u8]);
    fn poll(&mut self) -> Vec<Incoming>;   // connect / disconnect / message
    fn stats(&self, peer: PeerId) -> LinkStats;  // rtt, loss — feeds lag comp + UI
}
pub enum Channel { Reliable, Unreliable, UnreliableSequenced }
```

**Committed default: QUIC via `quinn`** — datagrams for the unreliable channels (raw-UDP
latency profile), a stream for the reliable channel, TLS encryption always-on (anti-tamper +
privacy for free), NAT-friendlier, and it is what `floptle-relay` and Cloud terminate. Ships
alongside `MemoryTransport` (loopback pipes — two worlds in one process for tests and the
editor's "Host & Join locally" debugging mode). The trait keeps `renet`/Steam/WebTransport
implementable later without touching gameplay.

## 6. Prediction & reconciliation — the one-script model

**The developer writes one controller script. It runs in three places unchanged:**

| Where | What it does |
|---|---|
| Server | authoritative sim, consuming the owner's *replicated* `InputCommand`s |
| Owning client | **predicted** sim, consuming *local* input immediately |
| Other clients | not run — the node interpolates snapshots |

The trick that makes this elegant: on a `Predicted` node, **`input.*` inside `fixedUpdate` is
role-aware plumbing** — the owning client reads the live device and records the snapshot; the
server reads the replayed `InputCommand` for that tick. The script cannot tell the difference
and never needs to. No `if net.isServer()` forest, no split codebase: `third_person.lua` as it
exists today becomes a working networked predicted controller by adding the Networked component.

Reconciliation is the standard rewind-replay, run by the engine:

1. Owner simulates tick `t` locally, stores `(input_t, predicted_state_t)` in a ring buffer.
2. Server snapshot arrives authoritative for tick `s`, acking input seq `a`.
3. Client compares its stored prediction at `s` with the server state. Within epsilon → done.
4. Else: reset the predicted node (+ its `replicated` vars + physics body) to server state at
   `s`, **replay** buffered inputs `a+1..now` through `fixedUpdate` + physics substeps, and
   smooth the residual visual error over ~100 ms so corrections read as a nudge, not a snap.

Replay cost is bounded (RTT × tick rate ≈ 6–12 ticks typical) and only touches the predicted
node's island, not the whole world.

## 7. Lag compensation — the parry recipe

The server keeps a **history ring of the last ~500 ms** of authoritative transforms + combat
state (`replicated` vars flagged as combat-relevant) for all networked entities, indexed by
tick. When a combat intent arrives from peer `P` stamped with the tick `P` *perceived* (their
interpolation timebase, which the server knows from `LinkStats` + the interp delay):

```lua
-- server-side handler; the engine rewinds the queries inside the closure
function onRpc.swing(args, peer)
  net.rewind(peer, function()
    local hit = raycast(args.ox, args.oy, args.oz, args.dx, args.dy, args.dz, params.reach)
    if hit and hit.node then
      local target = hit.node:getscript("combat")
      if target and target.synced.parrying then
        net.rpc("parried", { by = hit.node.id }, { to = peer })   -- you got parried
      else
        target.hurt(args.damage, peer)
      end
    end
  end)
end
```

`net.rewind(peer, fn)` re-poses networked colliders to `peer`'s perceived tick for the duration
of `fn` (raycasts and overlap queries inside see the rewound world), then restores. The
defender's `synced.parrying` flag is read *from the same rewound tick* — so a parry that was up
on the attacker's screen **counts**, which is the entire fairness contract of the genre. Rewind
depth is clamped (default ≤ 250 ms) so high-ping clients can't shoot into the distant past.

## 8. The Lua API — `net.*`

Complete v1 surface. Idiom-matched to the existing host (snake table global + camelCase
methods, `defaults`-style declaration tables, `onRpc` name dispatch like animation events).

### 8.1 Session

```lua
net.host{ maxPlayers = 16, port = 7777, interest = 150 }  -- become the authoritative host
net.join("quic://198.51.100.7:7777")                       -- direct / self-hosted
net.join("relay://lobby.mygame.dev/ABCD-1234")             -- via a floptle-relay lobby code
net.leave()

net.role()        -- "server" | "client" | "offline"
net.isServer()    -- convenience booleans
net.isClient()
net.peers()       -- server: list of connected PeerIds
net.ping(peer)    -- ms RTT (server: per peer; client: to server)
```

### 8.2 Lifecycle events

```lua
net.on("playerJoined", function(peer) ... end)   -- server
net.on("playerLeft",   function(peer) ... end)   -- server
net.on("connected",     function() ... end)       -- client: join completed
net.on("disconnected",  function(reason) ... end) -- client
```

### 8.3 Replication

```lua
-- code path (the Inspector "Networked" component is the same thing, zero-code)
node:replicate{ transform = true, physics = true }                      -- server-authoritative prop
node:replicate{ transform = true, physics = true,
                mode = "predicted", owner = peer }                      -- a player's avatar

-- per-script synced vars (see §4.3)
replicated = { health = 100, parrying = false }
-- ... then anywhere: synced.health = synced.health - dmg   (server-authoritative)
```

### 8.4 Spawning

```lua
-- server only; replicates to relevant clients automatically, returns the node
local avatar = net.spawn("scenes/player.ron", { x = 0, y = 2, z = 0, owner = peer })
net.despawn(avatar)
```

### 8.5 RPC

```lua
net.rpc("explode", { x = x, y = y, z = z })                -- server → relevant clients
net.rpc("chat", { msg = s }, { to = peer })                -- server → one client
net.rpc("buy_item", { id = 7 })                            -- client → server (intent)
net.rpc("swing", { dir = d }, { withInput = true })       -- client → server, tick-stamped
                                                           --   for lag comp (§7)
function onRpc.explode(args) spawnEffect("boom", args.x, args.y, args.z) end
function onRpc.buy_item(args, peer) ... end               -- sender peer on the server
```

### 8.6 Lag-compensated queries (server)

```lua
net.rewind(peer, function() --[[ raycast/overlaps see peer's perceived world ]] end)
```

### 8.7 Worked example — the whole multiplayer door

```lua
-- door.lua — attach to a node with the Networked component
replicated = { open = false }

function onRpc.use(args, peer)           -- client walked up and pressed E → net.rpc("use")
  if net.isServer() then synced.open = not synced.open end
end

function update(node, dt)                 -- cosmetic: everyone lerps toward synced truth
  local target = synced.open and 1.6 or 0.0
  node.y = node.y + (target - node.y) * math.min(1, dt * 6)
end
```

That's a fully server-authoritative, late-joiner-correct, interest-managed networked door in
ten lines, and it contains zero networking beyond one `rpc` and one `synced` read.

## 9. Rust architecture

```
floptle-net/
├── lib.rs         # NetSession: the one object the host loop drives
├── role.rs        # NetRole { Client, Server, ListenServer }
├── id.rs          # NetId allocation + Entity↔NetId maps
├── replicate.rs   # Replicated component, change detection, delta encode/decode
├── snapshot.rs    # baseline/delta snapshot ring, ack bookkeeping
├── interest.rs    # spatial grid + priority accumulator + byte budget
├── predict.rs     # input ring, rewind-replay driver, error smoothing
├── lagcomp.rs     # transform/state history ring + rewound-query scope
├── input_cmd.rs   # per-tick InputSnapshot capture, numbering, redundancy
├── rpc.rs         # named RPC registry, channels, intent piggybacking
├── transport/     # trait + MemoryTransport + QuicTransport (quinn)
└── wire.rs        # message framing, quantization (bincode/postcard + manual packing)
```

Integration seam: the editor's `play_step` (and later the headless runtime loop) drives
`NetSession::pre_tick(world, tick)` (apply received state/inputs) and
`NetSession::post_tick(world, tick)` (capture changes, build/send snapshots) around the
existing scripts→anim→physics ordering. The script host gains the `fixedUpdate` pass (§3),
`net`/`synced` globals, and the `Replicated` mirror — following the exact mirror-in /
queue-out / flush-after-run shape the host already uses for components, so the netcode never
holds `&mut World` across script execution.

Server builds: Phase 2 hosts from the editor (listen-server) and from `floptle-runtime
--headless` once its loop learns to run the real play step — the loop extraction from
`render_frame.rs` into a shared driver is scheduled inside Phase 2e, because dedicated servers
(and Ty's MMO ambitions) are pointless without it.

## 10. `floptle-relay` (open, self-hostable)

Small headless Rust binary (workspace crate, MIT/Apache like the engine):

- Terminates QUIC from all peers; forwards datagrams host↔clients (TURN-like). v1 is
  **relay-always** (simple, predictable); NAT hole-punch upgrade to direct P2P is a later
  optimization, not an MVP requirement.
- Minimal lobby service: host registers → gets a short code (`ABCD-1234`) → clients
  `net.join("relay://host/CODE")`. In-memory, no DB.
- One static config file (bind addr, TLS cert, lobby TTLs, per-IP rate limits). Deployable on
  any VPS. This is the honesty escape-hatch of ADR-0022 — Cloud is "this, managed."

## 11. Scale honesty

- **Phase 2 exit bar:** 4–16 player relayed/listen-server co-op, flawless feel at ≤120 ms RTT.
- **Same architecture, dedicated headless server + interest management on:** 50–150 players
  per instance is the realistic band (Rust-native sim, 30 Hz snapshots, budgeted interest —
  comfortably past typical Roblox experience ceilings, which is the stated goal).
- **"MMO"** beyond that = many instances + persistence + handoff, which is Cloud-era work
  (Phases 3–4) on top of this layer, not a bigger snapshot loop. The design keeps that door
  open (NetId space, interest sets, stateless relays) without promising sharding in v1.
- Parry windows: with 60 Hz tick + lag comp, fairness holds to ~150 ms RTT; beyond that the
  rewind clamp (§7) starts trading fairness-to-the-laggard for safety-of-everyone-else —
  the standard, correct tradeoff.

## 12. Phasing (each lands separately, playtested)

- **2a — Foundations:** gameplay tick + `fixedUpdate` + per-tick input; determinism audit;
  `NetId`; `Replicated` component + Inspector "Networked"; serializable body-state capture.
- **2b — Replication core:** `NetSession`, wire format, `MemoryTransport`; server-authoritative
  transform/`synced` replication + interpolation; `net.host/join`, spawn/despawn, RPC, events;
  the in-editor **"Host & Join locally"** harness (two simulated worlds in one process over
  loopback, latency/loss sliders) so every later stage is one-click testable.
- **2c — Prediction:** input commands, prediction ring, rewind-replay reconciliation, error
  smoothing; `third_person.lua` walks predicted with artificial 100 ms latency and feels local.
- **2d — Combat netcode** *(done)*: lag-comp history + `net.rewind` (poses AND `synced` vars
  rewound together, ~250 ms clamp); tick-stamped intents (`{withInput = true}`); body-hull
  raycasts with `hit.node` identity + caster self-exclusion (the missing substrate — rays
  couldn't hit players before); the parry recipe testable solo in the 2c harness (swing at a
  parrying dummy with the latency slider up). The two-client asymmetric-latency duel scene
  waits for 2e's real transport — the judgment path it would exercise is in and unit-tested.
- **2e — Transport & relay:** ~~`QuicTransport`~~ *(done: quinn behind the Transport seam —
  reliable = framed uni streams, unreliable = tagged datagrams w/ receiver-side sequencing +
  over-MTU reliable fallback; dev-trust self-signed TLS; editor `net.host{port}` /
  `net.join("quic://…")` with per-owner input routing on the host + client input-clock sync
  (welcome tick + RTT lead). v1 convention: scene-authored Predicted nodes belong to peer 1.)*
  Still to come: `floptle-relay` + lobby codes; headless `floptle-runtime` server loop;
  per-player avatar spawning w/ dynamic client-side predictor binding; interest management +
  byte budgets.
- **2f — Polish & docs:** scripting.md §"Networking", EmmyLua stubs + `.luarc.json` global +
  IDE completion/docs entries (all three doc surfaces), Console net-stats overlay, bandwidth
  profiler in the editor.

## 13. Decisions (resolved by Ty, 2026-07-06)

1. **Tick rate default = 60 Hz.** Parry timing wants the 16.6 ms input granularity; per-project
   configurable for games that want cheaper servers.
2. **`replicated` vars support tables from day one** — with guardrails so table sync can't
   become the large-blob footgun:
   - values: scalars + nested tables of scalars (arrays or string-keyed maps);
     **no functions/userdata/metatables** (rejected with a Console error at declaration);
   - **depth ≤ 4**, **encoded size ≤ 1 KB per var** (over-limit writes warn + drop, never
     silently truncate);
   - v1 sync granularity is **whole-value replace on change** (dirty-flagged per var);
     within-table deltas are a later optimization once real usage shows the shapes;
   - on `Predicted` nodes, table vars participate in rollback via per-tick copies — the 1 KB
     cap is what keeps that affordable.
3. **The in-editor "Host & Join locally" harness ships in 2b** (see §12) — loopback transport,
   two worlds in one process, latency/loss sliders.
4. **`floptle-relay` v1 is relay-always.** Simple, predictable, no NAT edge cases; direct-P2P
   hole-punch with relay fallback is a later optimization behind the same `relay://` URL.
