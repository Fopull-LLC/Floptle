# ADR-0014 — Mass/density-driven gravity as a field

- **Status:** Accepted · 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
Gravity in most engines is a single constant vector `(0,-9.8,0)`. The vision needs
much more: density is a property of **matter** (ADR-0013), and from density comes
**mass**, and from mass comes **gravity**. The engine should treat gravity as a
spatial **field** emitted by matter, enabling: running around on a **fractal**
surface (up a swirling wall, kept grounded by the field, not anti-gravity floaty),
**procedural fractal planets** with real pull, **spaceship** flight between
bodies, and **infinite fractal worlds**. Density should *also* govern whether
matter can be **crushed/compacted** under pressure (soft clay) or **resists**
(hard metal). It must be **opt-in and intuitive**, not mandatory per game.

## Decision
Gravity is a composable **vector field** `g(p)`, sampled per body as "down," built
from opt-in tiers (cheapest default → heaviest):

0. **Global / volume gravity** — a constant, or authored gravity regions. (Most games.)
1. **Analytic sources** — point/sphere/line sources (planets): Newtonian
   `g(p) = -G·M·(p-c)/|p-c|³`, or a fixed surface-normal pull. O(bodies×sources).
2. **SDF-surface gravity** — `g(p) ∝ -∇f(p)`: pulls bodies onto the nearest
   implicit surface, so you can **run on a fractal and up its swirling walls**.
   Reuses the SDF gradient physics already computes — nearly free. (The headline.)
3. **Calculated density-field gravity** — derive a density field `ρ(p)` from matter
   (per-material density × SDF occupancy), solve Poisson `∇²Φ = 4πGρ`, set
   `g = -∇Φ`; sampled cheaply, refreshed at low cadence.

The character controller samples `g(p)` and **aligns orientation to `-g`** →
wall-running, planet-walking, fractal-surface-walking. Density also sets **mass**
(`m = ρ·V`, volume from the SDF/mesh) and feeds **compaction** (bulk modulus +
yield) in the matter solver (ADR-0013).

## Why
- Unifies the four things the engine should "inherently understand": **space,
  matter, gravity, density.** The shared field substrate (`floptle-field`) already
  gives surface gravity almost for free.
- **Optimized by construction:** tiers + low-cadence solves + LOD; the expensive
  calculated tier uses **Barnes–Hut octree (O(N log N))** or a **grid FFT/multigrid
  Poisson** solve, not naïve O(N²) n-body.
- **Proven pieces:** local/curved gravity (Mario Galaxy, Outer Wilds); n-body
  treecodes/FMM and particle-mesh FFT Poisson solvers in computational physics.

## Alternatives considered
- **Global constant only** — can't express the fractal-walking / planets vision.
- **Real-time exact n-body** — O(N²), too slow; replaced by treecode/FFT approximations.

## Consequences
- Gravity lives as a `gravity` module in **`floptle-physics`**, consuming a density
  field from **`floptle-field`** (which now also holds scalar/vector spatial fields,
  not just SDFs). `matter` writes density; no dependency cycle.
- **Density becomes a first-class physical material property** (mass + compaction +
  gravity emission).
- Tiers 0–2 (incl. fractal-surface gravity) are near-term; the full **calculated
  density-field** tier for huge/infinite worlds is later/research.
- Full design + math: [`../subsystems/gravity-and-density.md`](../subsystems/gravity-and-density.md).
