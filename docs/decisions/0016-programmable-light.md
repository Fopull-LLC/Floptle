# ADR-0016 — Programmable light transport (light as a field)

- **Status:** Accepted · 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
The vision unifies space, matter, gravity, and density on one implicit-field
substrate. **Light** is the conspicuously missing peer, and the user wants "the
properties of light itself in the control of the developer." Crucially, the
renderer's headline loop *already is* light transport — a ray marched through
`f(p,t)` with the rule hardcoded to "straight line, vacuum, no absorption."

## Decision
Make light a **composable transport rule sampled along rays**, tiered like gravity
(ADR-0014), cheapest → heaviest, **off by default** so vanilla scenes are
bit-identical and pay nothing:

- **Tier 0 — Conventional:** straight rays + SDF soft-shadow/AO (today's renderer). Free.
- **Tier 1 — Programmable radiance hook:** replace the lighting equation per
  surface (`L(p, n, view, λ, t)`) — non-physical color, lighting driven by an
  authored scalar field ("mood"/"dream") instead of albedo·light.
- **Tier 2 — Bent transport (the headline, nearly free):** the rays the marcher
  already shoots get a per-step direction operator
  `dir = normalize(dir + bend(p, dir, t)·ds)` — curved shadow rays (impossible
  shadows), curved primary rays (lensing without mass). `bend` can **be `g(p)`**
  from the gravity field, so light falls under your custom gravity.
- **Tier 3 — Participating media (research):** accumulate emission/absorption
  along the march, with **signed** coefficients for "dark that emits."

Lives in `floptle-render` + additive shader-IR stdlib nodes; reads the shared
field. Composition is sum/compose of enabled tiers, exactly like gravity.

## Why
- Completes the thesis and is the most instantly-readable "another dimension" lever.
- **Nearly free** for the same reason SDF-surface gravity was: it reuses the march
  step and the SDF gradient already computed for normals.
- **Composes with gravity for free** (`bend = g(p)`) — one authored rule driving
  two unrelated phenomena because they're the same kind of object. Owning the
  shader IR (ADR-0007) makes light-rules additive nodes, not an engine rewrite.

## Alternatives considered
- **PBR / Lumen-style realism** — rejected; it's the opposite of our goal and far
  costlier. The "no GI-for-realism, stay single-march stylized" fence must hold.
- **Screen-space-only tricks** — already in the post stack, but limited to 2D.

## Consequences
- Bent secondary rays cost more and early-out less cleanly → must stay tiered/
  opt-in; honor `MAX_STEPS`, half-res, and the step-count profiler.
- Signed-coefficient media is order-dependent and NaN-prone → Tier 3 is research.
- Full design: [`../subsystems/light.md`](../subsystems/light.md).
