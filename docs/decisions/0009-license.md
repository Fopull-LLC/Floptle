# ADR-0009 — License & openness

- **Status:** Proposed · 2026-06-27 · *revised 2026-07-04*
- **Decider:** Ty Johnston (Fopull LLC)
- **Revised by:** [0022 — Networking & Floptle Cloud](0022-networking-and-cloud.md) broadens the
  revenue model below (donations → **hosted services + donations**) and adds a
  **permissive-forever** pledge plus a trademark policy — see the
  [networking proposal §9](../networking-proposal.md#9-licensing-trademark--the-permissive-forever-pledge).

## Context
The repository is **private now** — nothing public until the developer decides
it's ready. The long-term intent is **free and open-source** as a Fopull LLC
product. The original money model here was *donations only*; ADR-0022 broadens it
to **commercial managed services around the open engine** (Floptle Cloud) — the
"open engine + paid hosting" model (W4 Games / Godot), which keeps the engine free
while funding the business. Selling engine licenses remains off the table.

## Decision (proposed)
- Keep the GitHub repo **private** until launch.
- At launch, release under a permissive license. Per ADR-0022 the lean is now
  **dual MIT/Apache-2.0** (the Rust-ecosystem norm, as Bevy does), not Apache-2.0
  alone.
- **Permissive forever** — a public no-relicensing pledge; revenue comes from
  hosted services + donations, never from license terms (ADR-0022 §9).
- Protect the **"Floptle" trademark/branding** while the code stays free (the
  Godot/Blender model).

## Why
- **Permissive** licensing maximizes adoption and aligns with a
  services-and-donations model rather than license sales.
- **Dual MIT/Apache-2.0** gives the patent grant (Apache) and the familiarity/
  simplicity (MIT), and matches what Rust game-engine users already expect.
- A credible **no-BSL pledge** is a trust asset after the HashiCorp/Redis/Elastic
  relicensing backlashes.

## Open questions (resolve before going public)
- Confirm dual MIT/Apache-2.0 (recommended) vs a single license.
- Whether engine and a future "default content pack" carry the same license.
- Contributor terms (DCO sign-off vs a CLA) if outside contributions arrive.
- Exact trademark policy wording (name/logo use, fork naming).

## Consequences
- The workspace currently declares `license = "Apache-2.0"` as a placeholder;
  finalize here before the first public tag and add a top-level `LICENSE` file.
