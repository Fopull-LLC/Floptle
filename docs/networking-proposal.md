# Floptle Networking & Floptle Cloud — design proposal

- **Status:** Draft for review · 2026-07-04
- **Author:** Ty Johnston (Fopull LLC)
- **Companion ADR:** [0022 — Networking & Floptle Cloud](decisions/0022-networking-and-cloud.md)
- **Builds on:** [`subsystems/networking-future.md`](subsystems/networking-future.md) (the
  authoritative-server foundation) · [0021 Hub](decisions/0021-hub-and-distribution.md) ·
  [0009 License](decisions/0009-license.md)

## 1. What we're building, and the tension it resolves

Floptle is **free and open source**. That's non-negotiable — it's what earns adoption and
goodwill. But Fopull LLC is a business, and "open source" is not a business model. Selling
engine licenses would betray the openness; donations alone won't pay for servers or sustained
work.

The resolution is a well-worn, community-respected path: **keep the software free, sell the
managed service around it.** Concretely, we make multiplayer networking *excellent and built-in*
in the open engine, and we run the hard infrastructure — relays, matchmaking, dedicated hosting
— as an optional paid service, **Floptle Cloud**, hosted on Fopull's own servers (expandable to
a cloud provider later if it grows).

This is the same shape as:

| Open thing | Company selling the managed service |
|---|---|
| **Godot** (engine) | **W4 Games** (hosting, multiplayer, porting) |
| Unity's ecosystem | **Photon / PUN** (relay, matchmaking) |
| PostgreSQL | Supabase, Neon, RDS |
| Git | GitHub, GitLab |

The Godot/W4 case is the closest analog and the model to emulate: the engine is 100% free and
the money comes from *services people opt into*, not from the code.

## 2. The one principle that keeps us in good standing

> **Monetize the managed service, never the protocol.**

Everything else follows from this. The failure mode — the thing that earns resentment — is
lock-in: netcode that *only* talks to your servers, a closed wire protocol, no way to self-host.
Photon gets grumbling not because it charges money but because it's a closed box with pricing
that surprises people.

We do the opposite:

- **The networking API is open** and **transport-agnostic** — a game written against it runs
  over raw UDP, Steam sockets, a self-hosted relay, *or* Floptle Cloud, unchanged.
- **The wire protocol is documented and open**, and we ship an **open-source, self-hostable
  reference relay + matchmaker**. Anyone can run their own backend and never pay us a cent.
- **Floptle Cloud is the turnkey option** — same API, zero config, click-to-deploy from the Hub.
  You pay for **convenience and managed infrastructure**, not for a capability you'd otherwise
  lack.

The goodwill formula, stated plainly: **generous free tier + open protocol + transparent
pricing + a genuine self-host escape hatch.** If a studio *can* leave and chooses to stay
because ours is one-click and reliable, that's a business the open-source community respects.

## 3. Two layers, one clean seam

There are two deliverables with fundamentally different licenses and audiences:

```
 ┌─────────────────────────────────────────────────────────────┐
 │  A. ENGINE NETCODE  (open source, in-repo, MIT/Apache)        │
 │     replication · RPC · roles · prediction · Transport trait  │
 │     + a self-hostable open reference relay (floptle-relay)    │
 └───────────────────────────────┬─────────────────────────────┘
                                  │  the Transport trait is the seam
                                  ▼
 ┌─────────────────────────────────────────────────────────────┐
 │  B. FLOPTLE CLOUD  (proprietary service, separate repo)       │
 │     managed relay · matchmaking · dedicated hosting · regions │
 │     billed via the Hub + fopull.com — an *implementation* of  │
 │     the same open Transport, not a fork of the API            │
 └─────────────────────────────────────────────────────────────┘
```

The boundary is the `Transport` trait (§4.3). Floptle Cloud is *one implementation* of it,
sitting alongside `UdpTransport`, `SteamTransport`, and `SelfHostedRelayTransport`. That single
design choice is what makes the whole thing non-lock-in by construction.

## 4. Layer A — the open engine netcode

This inherits the foundation already designed in `networking-future.md` and adds the API polish
and relay topology. Nothing here is Cloud-specific.

### 4.1 Roles and topologies

One codebase, three roles chosen at build/runtime (from `networking-future.md`):

```rust
enum NetRole {
    Client,        // connects to a remote authoritative host
    Server,        // headless authoritative host (the "server build")
    ListenServer,  // a player's machine hosts + plays (cheap co-op)
}
```

We support **both** topologies the engine will ever need, and the same replication API runs on
both — the difference is only *who is authoritative* and *how bytes get there*:

- **Authoritative dedicated** — a headless server build owns the truth; clients predict + render.
  Best for competitive/persistent games. (Designed in `networking-future.md` §1–§5.)
- **Relayed peer / listen-server** — one peer is authoritative (host), others connect through a
  relay that solves NAT. Best for co-op and casual. **This is what Cloud ships first** (§6).

### 4.2 What replicates (recap)

Only **networked** components on **replicated** nodes, with explicit authority — see
`networking-future.md` §3 for the full model (`Replicated { id, authority, components, interp }`,
`Authority::{ Server, Client(PeerId) }`, delta snapshots + input commands, client prediction &
server reconciliation over the fixed-timestep deterministic sim). Visual-only state (most VFX,
post) never replicates — it re-derives from synced params + time.

### 4.3 The transport seam

The socket lives behind one trait so the choice is swappable and gameplay never sees it (from
`networking-future.md` §4):

```rust
trait Transport {
    fn send(&mut self, peer: PeerId, channel: Channel, bytes: &[u8]);
    fn poll(&mut self) -> Vec<Incoming>;      // connects / disconnects / messages
}

enum Channel { Reliable, Unreliable, UnreliableSequenced }
```

Implementations (engine ships the first two open; Cloud provides the third):

- `UdpTransport` / `QuicTransport` — direct connect (self-host, LAN, known address). Likely
  `renet`/`laminar` (UDP, game channels) or `quinn` (QUIC — encrypted, NAT-friendlier, matches
  the "deploy behind TLS on my infra" model). Protocol choice is an open question (§12).
- `SelfHostedRelayTransport` — connects through **`floptle-relay`**, the open reference relay.
- `CloudTransport` — connects through Floptle Cloud's managed relay/matchmaker. *Same trait.*

### 4.4 The game-facing API (Lua + Rust)

Networking must be as easy to reach as physics and particles already are (the engine's whole
selling point). A first sketch of the Lua surface, mirroring the existing `node:getcomponent`
/ lifecycle style:

```lua
-- host or join, transport chosen by config (direct / self-relay / cloud)
net.host{ max_players = 4 }                 -- become authoritative (listen-server)
net.join("floptlecloud://ABCD-1234")        -- join via a Cloud lobby code
net.join("udp://198.51.100.7:7777")         -- or a direct/self-hosted address

-- mark a node's state as replicated (authority defaults to the host)
node:replicate{ transform = true, script_vars = {"health", "ammo"} }

-- remote calls
net.rpc("spawn_pickup", { x = 3, y = 0, z = 1 })   -- server → clients
function on_rpc.spawn_pickup(args) ... end

net.on("player_joined", function(peer) ... end)     -- lifecycle events
```

The Rust API underneath is the real thing; Lua is the easy front door. **Ease-of-use is the
product** — a built-in `net.host{}` that "just works" (including NAT, via Cloud) is what drives
people to the paid relay.

### 4.5 The self-hostable reference relay (`floptle-relay`)

Open source, in-repo, MIT/Apache. A small headless Rust binary that:

- Accepts peer connections, does NAT hole-punching, and relays packets between them (a TURN-like
  fallback when direct punch fails).
- Runs a minimal lobby/matchmaking list (create lobby → get a code → others join by code).
- Is deployable anywhere the dev wants (their own box, a VPS) with a config file.

This is the **escape hatch that makes the whole model honest**: Floptle Cloud is "this same
relay, but we run it, scale it, and give it regions + a dashboard." Shipping the reference relay
open is a feature, not a giveaway.

## 5. Layer B — Floptle Cloud (the product)

A proprietary managed backend on Fopull's infrastructure. It is an *implementation* of Layer A's
open transport + a control plane, not a different engine API.

### 5.1 What the customer actually pays for

Not "the ability to do multiplayer" (that's free and open) — but the **infrastructure and
operations** they'd otherwise run themselves:

- **Managed relay** — always-on relays with NAT traversal, so `net.host{}` works for players
  behind home routers with zero setup.
- **Matchmaking** — lobby lists, skill/region matching, join-by-code, quick-play.
- **Dedicated authoritative hosting** *(product #2)* — upload your server build; we run it in a
  region, keep it up, restart on crash, and scale instances.
- **Regions & routing** — low-latency edges; expand from Fopull's server to a cloud provider
  (AWS/etc.) as demand grows.
- **Persistence** *(later)* — a managed key/value or player-profile store for saves/leaderboards.
- **DDoS protection & abuse handling** — the unglamorous operational work.

### 5.2 Product sequencing

1. **Relay + matchmaking (ship first).** Cheapest infra (stateless relays), highest immediate
   value (NAT is *the* blocker for casual multiplayer), fastest to a paid tier.
2. **Dedicated server hosting (second).** Upload a headless server build (the export path from
   `networking-future.md` §5), we run it. Bigger product, more ops, more revenue.
3. **Persistence / profiles / leaderboards (later).** Managed stateful services.

### 5.3 Pricing shape (illustrative — see §12)

Modeled on what the community accepts (Photon-style CCU tiers, but transparent and with a real
free tier):

| Tier | Who | Roughly |
|---|---|---|
| **Free / self-host** | hobbyists, self-hosters | run `floptle-relay` yourself, or a small always-free Cloud allowance (low CCU, one region, rate-limited) |
| **Indie** | small paid games | monthly: higher CCU, multi-region relay, matchmaking |
| **Studio** | bigger titles | dedicated server hosting, more instances/regions, priority |
| **Metered** | overage | pay-as-you-go CCU / bandwidth above the tier |

The free tier and the self-host path are **load-bearing for goodwill** — they must be genuinely
useful, not a crippled demo.

## 6. The Hub as the control panel

The Hub (ADR-0021) already installs versions and manages projects; it's the natural home for the
account and Cloud surface. New Hub responsibilities:

- **Account** — sign in to fopull.com (§7), show subscription/entitlements, sign out.
- **Multiplayer / Cloud tab** — provision a relay/lobby, view usage, get a connect string, and
  (product #2) **Deploy** a server build to a region with one click.
- **Billing shortcut** — deep-link to the fopull.com billing portal (Stripe-hosted; the Hub
  never handles card data).

Everything stays optional: a user who never signs in still gets the full open engine, the Hub,
and self-hosted networking.

## 7. Identity & billing on fopull.com

fopull.com becomes the **identity provider + billing system**; the Hub and engine are OAuth
clients.

- **Auth: OAuth 2.0 Device Authorization Grant** (RFC 8628) — the native-app standard. The Hub
  shows a code / opens the browser; the user approves on fopull.com; the Hub receives tokens.
  No passwords ever touch the native app.
- **Token storage: OS keyring** (the "later hardening step" from the Hub work lands here).
  Access tokens are **host-scoped** — sent only to fopull.com / Floptle Cloud hosts, never
  attached to arbitrary URLs (the exact discipline already used for the GitHub manifest token).
- **Billing: Stripe** (Checkout + Customer Portal + webhooks). Fopull stores *entitlements*, not
  cards. A webhook flips a subscription; entitlements become token scopes the Cloud honors.
- **Entitlements → capability** — the Cloud relay/matchmaker checks the token's scope (tier, CCU
  ceiling, regions) on connect. The *engine* never enforces payment; only the managed service
  does. Self-hosting bypasses all of it, by design.

## 8. What the engine already gives us for free

The single-player decisions make networking dramatically cheaper (from `networking-future.md`
§2): the **fixed-timestep deterministic sim** is the bedrock of prediction/reconciliation; all
component state is already **serializable** (RON/ECS); and `floptle-net` is a **clean seam** so
gameplay is transport-agnostic today. We are not starting from zero — we're filling in a
foundation the architecture was already shaped for.

## 9. Licensing, trademark & the permissive-forever pledge

This section refines [ADR-0009](decisions/0009-license.md) for the new revenue model. (No
`LICENSE` file is committed yet — the repo is private until launch — but this is the committed
*intent*.)

- **Engine license: MIT/Apache-2.0 dual**, the Rust-ecosystem norm (what Bevy does). Maximum
  adoption, patent grant (Apache), and familiarity (MIT). Add both `LICENSE-MIT` and
  `LICENSE-APACHE` at first public tag.
- **Permissive forever — a public no-relicensing pledge.** We will *not* move the engine to a
  source-available license (BSL/SSPL) to claw back the service business. The HashiCorp / Redis /
  Elastic relicensings are the cautionary tale; a credible "we won't do that" is a real trust
  asset. Revenue comes from **hosted services + donations**, never from license terms.
- **Trademark / branding policy** (Godot/Blender model): the *code* is free; the **"Floptle"
  name and logo are protected.** Forks are free to exist but can't call themselves "Floptle" or
  imply endorsement. This is how the open project drives traffic and trust to fopull.com without
  the code being a lock-in — the goodwill *and* the funnel, honestly separated.
- **Floptle Cloud is proprietary** — it's a hosted service, not distributed code, so it carries
  no obligation to open it. The **reference relay is open** (§4.5), which is what matters for the
  no-lock-in promise.
- **Contributions** — DCO sign-off (lightweight) over a CLA, unless a CLA becomes necessary; the
  permissive license already lets Fopull use contributions in the commercial context.

## 10. Security & anti-cheat posture

- **Authoritative server rejects illegal client mutations** by construction (the authority model,
  §4.2) — never trust the client. Validation, rate limits, and encrypted transport (QUIC) harden
  it (`networking-future.md` §6).
- **Auth tokens**: keyring-stored, host-scoped, short-lived access + refresh; revocable
  server-side.
- **Cloud abuse**: relay rate-limits and per-account quotas; DDoS mitigation at the edge; payment
  fraud handled by Stripe.
- **Depth for v1 is an open question** (§12) — start with authority + rate limits; deeper
  anti-cheat (server-side lag comp validation, integrity checks) is sequenced later.

## 11. Phased roadmap

Networking is a **large, multi-quarter subsystem** — bigger than the Hub or particles. It starts
opinionated and small, not "all of netcode at once."

- **Phase 0 — this doc.** Lock the shape (ADR-0022 + this proposal).
- **Phase 1 — Hub login + accounts.** OAuth device flow, keyring, account status in the Hub;
  fopull.com as OIDC provider. *Independently useful; the substrate for everything paid.* No
  netcode yet.
- **Phase 2 — engine netcode MVP (open).** Grow `floptle-net`: `Transport` trait, authoritative
  snapshot replication + input commands + basic prediction, the Lua `net.*` API, and the
  open **`floptle-relay`** reference (relayed listen-server co-op working end-to-end,
  self-hosted).
- **Phase 3 — Floptle Cloud: relay + matchmaking + billing.** Managed relay/matchmaker, Stripe
  tiers, entitlement scopes, the Hub Cloud tab. **First revenue.**
- **Phase 4 — dedicated server hosting.** Headless server export → deploy from the Hub → managed
  instances, regions, restart-on-crash. Persistence/profiles follow.

Each phase is a normal, reviewable chunk; we stop and playtest between them.

## 12. Open questions

- **Free-tier limits & pricing** — what's generous-but-sustainable? (CCU ceiling, regions,
  bandwidth.)
- **Wire transport** — QUIC (`quinn`, encrypted/NAT-friendly) vs UDP (`renet`, lighter, game
  channels) as the default; likely QUIC for Cloud, UDP option for LAN/self-host.
- **Matchmaking API shape** — lobby codes only for v1, or skill/region matching too?
- **Anti-cheat depth for v1** — authority + rate limits only, or server-side validation from the
  start?
- **Persistence** — do we offer a managed store in the first Cloud year, or stay compute-only?
- **License final call** — dual MIT/Apache-2.0 (recommended) confirmed; content-pack license;
  DCO vs CLA.
- **Cloud repo** — private monorepo section vs a separate repository for the proprietary backend.
