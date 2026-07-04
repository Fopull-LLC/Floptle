# ADR-0022 — Networking & Floptle Cloud

- **Status:** Proposed · 2026-07-04
- **Decider:** Ty Johnston (Fopull LLC)
- **Detail:** [Networking & Floptle Cloud proposal](../networking-proposal.md)
- **Relates to:** [0009 — License & openness](0009-license.md) (refines the revenue model),
  [0021 — Hub & distribution](0021-hub-and-distribution.md) (the Hub becomes the account/deploy
  surface). Inherits the technical foundation of, and **supersedes the "not a platform" scope
  of**, [`../subsystems/networking-future.md`](../subsystems/networking-future.md).

## Context
The engine is **free and open source** (ADR-0009). Fopull LLC still needs a revenue path, and
selling engine licenses is off the table — it contradicts the openness that drives adoption.
Donations alone won't fund server infrastructure or sustained development.

Multiplayer networking is the natural monetizable surface. The *API* is engine code, but
running it well — NAT traversal, relays, matchmaking, dedicated authoritative hosting, DDoS
protection, regions — is **real infrastructure people will gladly pay to not operate
themselves.** This is the proven "open engine + commercial hosted services" model: **W4 Games
for Godot**, and the role **Photon** plays for Unity.

`floptle-net` is a boundary stub today; `networking-future.md` already designed the
authoritative client-server foundation but deliberately scoped *out* a managed/hosted service
("your server, your infra — not a platform"). The business direction now **includes** a managed
service. This ADR evolves that stance without weakening the open foundation.

## Decision
1. **The netcode API stays open and transport-agnostic.** The engine ships the full
   authoritative client-server model (`NetRole`, `Replicated`, a `Transport` trait — inherited
   from `networking-future.md`) **and** an open-source, self-hostable **reference relay +
   matchmaker**. No networking capability is gated behind the cloud.
2. **Monetize the managed service, not the source.** *Floptle Cloud* is a proprietary hosted
   backend that speaks the same open `Transport`. Paid buys **managed convenience and infra**
   (zero-config relays, matchmaking, dedicated hosting), never capability a self-hoster lacks.
3. **Design for both topologies; ship relay first.** Relay + matchmaking (P2P & co-op — cheapest
   infra, solves NAT, fastest to a paid tier) is Cloud product #1; managed authoritative
   dedicated hosting is product #2. The engine API abstracts over both from day one.
4. **The Hub is the account + deploy + billing surface.** OAuth 2.0 **device-authorization**
   login to fopull.com, token in the **OS keyring**; provision/connect/deploy from the Hub;
   Stripe billing on the website. No passwords in the native app; tokens are host-scoped.
5. **Licensing commitment (refines ADR-0009).** The engine stays **permissive (MIT/Apache-2.0
   dual)** — **forever**, as a public no-relicensing pledge. A **trademark/branding policy**
   protects "Floptle." The Cloud backend is proprietary (a service, not distributed code).
   Revenue = **hosted services + donations**, never license sales.

## Why
- **Open protocol + a real self-host path = no lock-in = community respect.** The W4/Godot
  precedent shows this earns goodwill; Photon's grumbling comes from a closed protocol + surprise
  pricing. We avoid both with a **generous free tier, transparent pricing, and a genuine
  self-host escape hatch.** People pay because ours is one-click, not because they're trapped.
- **Relay-first** is the cheapest infra with the highest immediate value — NAT traversal is the
  #1 multiplayer pain — so it's the shortest path to sustainable revenue.
- **The Hub already is the control panel;** login, deploy, and billing belong there, not in a
  separate app.
- **Permissive-forever + trademark** is the Godot/Blender goodwill formula, and an explicit
  anti-BSL stance after the HashiCorp/Redis relicensing backlashes.

## Alternatives considered
- **Closed netcode / cloud-only transport** — maximizes revenue capture, but lock-in destroys
  OSS standing. Rejected: it's the exact thing Ty wants to avoid.
- **Donations only** (ADR-0009 as written) — respected, but doesn't fund infrastructure or
  salaries. Insufficient on its own; kept as a *secondary* channel.
- **Relicense to source-available (BSL/SSPL) later** — the HashiCorp/Redis/Elastic backlashes
  show the cost to trust. Explicitly rejected via the permissive-forever pledge.
- **Default to a third-party backend (Photon/Nakama/Edgegap)** — cedes both the monetization
  surface and the built-in UX. We want networking to be *built-in, easy, and ours* — while
  leaving third parties pluggable via the open `Transport`.

## Consequences
- **New surfaces:** grow `floptle-net` (replication + transport), an open `floptle-relay`
  reference server, a **Floptle Cloud** backend (private, separate repo), Hub auth + deploy UI,
  and website identity (OIDC) + Stripe billing.
- **Netcode is a large, multi-quarter subsystem** (prediction, reconciliation, lag comp,
  anti-cheat). It starts opinionated and small (relayed co-op + authoritative snapshot
  replication), not "everything on day one."
- Going public now additionally requires an actual **LICENSE file + trademark policy** (deferred
  per ADR-0009; tracked in the proposal §9).
- **Security surface** grows: auth tokens (keyring, host-scoped), relay/server abuse + DDoS,
  and payment data (offloaded entirely to Stripe — we never store cards).
- **Open (see the proposal §12):** free-tier limits, pricing shape, wire protocol (QUIC vs UDP),
  the matchmaking API, and how deep anti-cheat goes for v1.
