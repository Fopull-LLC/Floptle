# ADR-0019 — Field-interaction graph (composition by design)

- **Status:** Accepted · 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
The substrate unifies **reading** — render and physics both sample one `f(p,t)` —
but the gameplay fields currently **compose only by coincidence**: every field is
a one-way reader. ADR-0014 deliberately cut the density↔gravity cycle to keep
crate dependencies acyclic. That is **correct for code**, but it must **not leak
into the simulation layer**, where it would foreclose the feedback loops that
create *un-authored surprise* — the difference between "a stack of impressive
demos" and "a universe whose surprises you didn't individually author." The
"matrix god" promise is specifically about defining how forces **interact**.
Retrofitting feedback into a one-way architecture is nearly impossible, so the
**seam must exist before systems calcify.**

## Decision
Add a typed **field-interaction graph**: "field A modulates field B" is an
authored **edge**, iterated at **low cadence with explicit damping**, kept
**architecturally distinct** from crate dependencies. Existing couplings become
default edges (density→gravity, sdf-gradient→gravity, gravity→matter). Pair it
with a **"rule-lens" legibility overlay** (visualize any field as color/arrows) —
*emergence without legibility is noise*, and the engine's determinism makes a
discovered trick repeatable (the precondition for mastery, not luck). The graph
**data** lives in `floptle-rules`; the **executor** is a runtime system.

## Why
- Generalizes the material-interaction matrix (already the most emergence-shaped
  thing in the engine) from "geometry blend" to "any field modulates any field."
- Turns coincidental composition into **designed** composition — the layer that
  makes the world worth *inhabiting*, not just impressive to look at.

## Discipline (from the advisory)
Build the **seam + ONE proof**, not the full graph: heat flows into a fractal wall
→ warm bricks lower their SDF toward soup (temperature→sdf) → it melts where heated,
auto-walkable (shared field) → molten density drops so surface gravity weakens and
you slide off (density→gravity). Every piece already exists; you add the **edges**,
not subsystems. Default to a **shallow, damped, mostly-authored** coupling set
(the authored-dream vs. emergent-sim question is a vision call — see
[`../subsystems/field-interaction.md`](../subsystems/field-interaction.md) §7).

## Alternatives considered
- **Keep one-way readers** — no emergence, and nearly impossible to retrofit later.
- **Full conserved-transport sim** (heat diffusion/advection, etc.) — heavy/
  research, and trends toward *realistic* equilibria that fight the surreal intent.

## Consequences
- Keep the runtime graph separate from crate deps (conflating them yields either a
  compile-time impossibility or an under-damped sim).
- Damping + fixed iteration order to stay tame and deterministic.
- Full design: [`../subsystems/field-interaction.md`](../subsystems/field-interaction.md).
