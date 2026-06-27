# Floptle — Networking (`floptle-net`, deferred / future)

> **Forward-looking design, not a launch requirement.** `floptle-net` is a
> boundary stub today; this doc records the target so the engine keeps the right
> seams. See [`../ARCHITECTURE.md`](../ARCHITECTURE.md) §10, the deterministic sim
> in [`./physics.md`](./physics.md), and [`../ROADMAP.md`](../ROADMAP.md) "Later".

This is **explicitly deferred** — sequenced *after* the single-player engine is
solid. We design for it now only enough to avoid painting ourselves into a corner.
The motivation is concrete and the developer's own: Ty self-hosts a server
(website, games, business tools) and wants Floptle games to *optionally* network on
**his own infrastructure** — make a server build, define its behavior, deploy it
anywhere, and let clients connect. No third-party netcode service, no required
cloud.

## 1. The target model

**Authoritative dedicated server + connecting clients.** The server owns the
truth; clients send input and render predicted/reconciled state. Three roles from
one codebase, chosen at build/runtime:

```rust
enum NetRole {
    Client,        // connects to a remote authoritative server
    Server,        // headless authoritative host (the "server build")
    ListenServer,  // a player's machine is also the host (cheap co-op)
}
```

```
        ┌──────────────────────────────────────────┐
        │   SERVER build (headless floptle-runtime)  │  ← authoritative sim
        │   self-hosted on the dev's infrastructure  │
        └───────────────┬───────────────┬───────────┘
            snapshots ▲  │               │  ▲ snapshots
            input cmds│  ▼               ▼  │input cmds
              ┌───────┴─────┐      ┌──────┴──────┐
              │  CLIENT A   │      │  CLIENT B   │   ← predict + render
              └─────────────┘      └─────────────┘
```

## 2. Why the architecture already leans this way

We get most of the hard prerequisites for free from decisions made for
single-player reasons:

- **Fixed-timestep deterministic sim** — gameplay/physics already step on a fixed
  accumulator ([ARCHITECTURE](../ARCHITECTURE.md) §3, [`./physics.md`](./physics.md)).
  Determinism is the foundation of prediction + reconciliation and of cheap
  lockstep options.
- **Serializable component state** — everything authored is RON, and component
  data is plain data in the ECS ([ADR-0005](../decisions/0005-scene-model-ecs-node-hybrid.md)),
  so state is already (de)serializable to RON or a compact binary form.
- **A clean seam in `floptle-net`** — gameplay code is written against an
  authoritative-update model and never talks to a socket directly, so it stays
  **transport-agnostic**. Adding networking should not reshape gameplay code.

## 3. What replicates

Not everything syncs — only **networked** components on **replicated** nodes, with
an explicit authority.

```rust
struct Replicated {
    id:         NetId,         // stable network id (distinct from local AssetId)
    authority:  Authority,     // Server | Client(peer) — who may mutate
    components: ComponentMask, // which components on this node sync
    interp:     bool,          // smooth on clients between snapshots
}

enum Authority { Server, Client(PeerId) }
```

- **Replicated state** — transforms, gameplay components (health, inventory,
  animation state), and the SDF-world params that morph the level. Visual-only
  noise (most VFX, post effects) need not replicate — they re-derive from synced
  params + time.
- **Ownership / authority** — the server is authoritative by default; a client may
  hold authority over its own input/avatar intent. Mutations from a non-authority
  are rejected (the anti-cheat posture, §6).
- **Client prediction + server reconciliation** — the client simulates locally
  from its own input immediately, tags each step with an input sequence number,
  and on each server snapshot **rewinds and replays** unacknowledged inputs from
  the authoritative state. Deterministic fixed-step sim makes this clean.
- **Snapshot / delta vs input-command** *(sketch, not committed)*:

```
client ──▶ InputCommand { seq, tick, actions }  ─────────────▶ server
server ──▶ Snapshot { tick, ack_seq, [delta components] } ───▶ client
                         │                          │
                  baseline + delta            replay inputs > ack_seq
```

Servers send periodic **delta snapshots** (changed components since a baseline);
clients send compact **input commands**. Exact wire model is chosen when we build
this, not now.

## 4. Transport options (later, behind a trait)

The socket lives behind one trait so the choice is swappable and gameplay never
sees it:

```rust
trait Transport {
    fn send(&mut self, peer: PeerId, channel: Channel, bytes: &[u8]);
    fn poll(&mut self) -> Vec<Incoming>;   // connects/disconnects/messages
}

enum Channel { Reliable, Unreliable, UnreliableSequenced }
```

Candidates, evaluated when the time comes:

- **UDP** via `renet` or `laminar` — game-oriented, channels with optional
  reliability, mature in the Rust ecosystem.
- **QUIC** via `quinn` — encrypted, multiplexed streams, NAT-friendlier; heavier
  but nice for the dev's "deploy on my own infra behind TLS" model.

No commitment now — only the trait seam exists.

## 5. The server build & deployment

A **server build** is the headless path through `floptle-runtime` ([ROADMAP](../ROADMAP.md)
Phase 9 export): no window, no renderer, no editor — just the deterministic sim,
scripts, physics, and the net layer in `NetRole::Server`. The developer **defines
the server's behavior** (which scenes, rules, tick rate, max peers) in
`project.ron` / a server config, then deploys the binary anywhere on their
self-hosted infrastructure.

```
floptle project ─▶ export (server profile) ─▶ headless floptle-runtime binary
                                                   │ deploy on dev's infra
                                                   ▼
                                          listens · authoritative sim
                                                   ▲
                            non-server (client) builds discover + connect
```

Client discovery starts simple: connect to a configured address (the dev knows
their own server). A lightweight master/list or LAN broadcast is a possible later
nicety, not a requirement.

## 6. Later concerns (noted, not designed)

Sequenced even further out:

- **Interest management / scale** — only replicate what a client can perceive
  (spatial grid / relevancy) once player counts justify it.
- **Anti-cheat posture** — the authoritative server already rejects illegal
  client mutations (§3); validation, rate limits, and encrypted transport (QUIC)
  harden it later. Never trust the client.
- **Lag compensation / rollback netcode** flavors, snapshot interpolation tuning,
  and bandwidth budgeting — all deferred until a real game needs them.

## 7. Scope & sequencing

This is **after** the single-player engine is solid ([ROADMAP](../ROADMAP.md)
"Later"). Until then, `floptle-net` ships as a **boundary stub** that holds the
seam — gameplay compiles and runs single-player exactly as if networking didn't
exist, and the deterministic, serializable foundation quietly keeps the door open.

Out of scope for the foreseeable future: matchmaking services, a hosted/managed
multiplayer backend, voice chat, and MMO-scale sharding. Floptle networking means
**your server, your infra, your rules** — not a platform.
