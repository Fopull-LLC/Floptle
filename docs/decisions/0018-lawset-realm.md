# ADR-0018 — The Lawset / Realm meta-spine

- **Status:** Accepted · 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
The thesis is "a game is a simulated universe with developer-defined **rules**."
But today those rules are scattered — gravity on a component, light in the
renderer, time as a global, matter on materials — with **no single object meaning
"the laws of this world."** A multi-lens advisory pass independently converged on
this same missing primitive from four directions (light, time, world-rules,
emergence): the engine's substrate was unknowingly built toward one container.

## Decision
Introduce a first-class, serializable, **inheritable `Lawset`** (the set of laws)
bound to an SDF volume (a **`Realm`**). "Which laws hold at `p`" is resolved by the
**inside-test the engine already does**; child realms **override parent laws
axis-by-axis**, and scalar laws **crossfade at `smin` boundaries** (so a law
changes smoothly as you cross into a realm). Axes are the law dimensions we've
designed — gravity model, light model, time rate, matter/material rules, scale
(space/portal axis is later). Each axis is a lean **enum of 3–4 named MODELS**
plus `Inherit`; **absent laws cost nothing**; resolve **once per body per step and
cache**.

New **thin, pure-data** crate **`floptle-rules`** (depends on `core` + `field`;
**read-only** to render/physics/matter, so no dependency cycles). A Floptle world
becomes a `lawset.ron` you can diff, hot-reload, hand to an AI, and gift as "here
are the laws — bend them."

## Why
- Makes the thesis *buildable* and becomes Floptle's identity and its open-source
  statement in code. No incumbent has a single object meaning "the rules of this
  universe."
- Absorbs the new asks (light, time) as just two more **law-axes**, gives every
  system **one authority** to read instead of carrying copies, and avoids the
  property-soup the VISION explicitly rejects via lean models + `Inherit`.

## Discipline (from the advisory — this matters)
Build it **now but THIN**: a Lawset struct + a resolver + caching, wiring only the
axes you can prove first (gravity moved off the per-body component to a per-realm
default, and `time.scale`). **Do not** big-bang the full multi-axis system before
the look is proven — the spine exists to keep the *seams* right cheaply, never to
compete with getting pixels on screen.

## Alternatives considered
- **Scattered per-system rules** (status quo) — can't express "a universe"; and
  retrofitting a spine later, after systems calcify around one-way assumptions, is
  far harder.
- **A giant settings UI** — the property-soup the VISION rejects.

## Consequences
- One resolver on the hot path (mitigated by per-body/step caching).
- Must stay low-layer and pure-data to avoid dep cycles.
- Full design: [`../subsystems/world-rules.md`](../subsystems/world-rules.md).
  Cross-field *coupling* is its sibling, ADR-0019.
