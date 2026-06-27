# ADR-0009 — License & openness

- **Status:** Proposed · 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
The repository is **private now** — nothing public until the developer decides
it's ready. The long-term intent is **free and open-source with a donations
option**, as a Fopull LLC product.

## Decision (proposed)
- Keep the GitHub repo **private** until launch.
- At launch, release under a permissive license — **Apache-2.0** (current lean),
  with **MIT** as the alternative.

## Why
- **Permissive** licensing maximizes adoption and aligns with a donation model
  rather than license sales.
- **Apache-2.0** adds an explicit patent grant and contributor terms; **MIT** is
  simpler and more familiar. Either fits "free and open."

## Open questions (resolve before going public)
- Apache-2.0 vs MIT (vs dual MIT/Apache, the common Rust convention).
- Whether engine and a future "default content pack" carry the same license.
- Contributor terms (DCO sign-off vs a CLA) if outside contributions arrive.

## Consequences
- The workspace currently declares `license = "Apache-2.0"` as a placeholder;
  finalize here before the first public tag and add a top-level `LICENSE` file.
