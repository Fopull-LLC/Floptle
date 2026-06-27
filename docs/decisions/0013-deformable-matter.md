# ADR-0013 — A unified deformable-matter substrate

- **Status:** Accepted · 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
Fractals already morph (ADR-0012). But the vision is bigger: **nothing should be
a static box.** A developer's own shapes/meshes should also shift and morph their
vertices in-engine and stay **cleanly collidable**; geometry should be able to
**blend together, mix, or reject** other geometry; objects should optionally have
**soft-body** behavior, **stick** to surfaces and **stretch** when pulled, and —
later — **tear into stringy strands and split apart**. One central idea:
everything is changeable, controllable, movable — matter, not props.

Traditional engines split a thing into three desynced representations (render
mesh / collision shape / physics body), which makes unified, topology-changing
deformation fight the whole stack.

## Decision
Make **implicit fields the shared substrate** and add a dedicated
**deformable-matter** system on top:

- New crate **`floptle-field`** — SDFs + CSG/blend operators (`smin`/`smax` with a
  blend radius, per-material-pair blend *rules*) + mesh↔SDF conversion. One
  representation that the **renderer raymarches/meshes** and **physics collides
  against** — so a deformed object is renderable *and* collidable with no sync.
- New crate **`floptle-matter`** — a single `MatterModel` component with **opt-in
  tiers**, cheapest→heaviest: **Rigid → Morph** (GPU vertex/field displacement)
  **→ FieldBlend** (CSG soup/mix/reject) **→ SoftBody** (XPBD) **→ Viscoelastic**
  (adhesion/sticking + stretch-to-fracture). You only pay for the tier you reach
  for — same philosophy as the rest of the engine.

## Why
- The field substrate turns "blend / mix / reject" into **algebra** (`smin`/`smax`),
  and keeps deformation collidable for free (physics already evaluates `f(p,t)`).
- Tiers keep it **hyperoptimized**: most objects are Rigid/Morph (≈free); only
  objects that need it pay for XPBD/fracture, and only while interacted with.
- It's **proven**: Media Molecule's *Dreams* (SDF sculpt + sim) and *Claybook*
  (deformable SDF clay + physics) ship this paradigm; Disney's MPM snow shows the
  heavyweight "soup" path. Floptle aims this at *surreal*, not realism.

## Alternatives considered
- **Mesh-only + a bolted-on soft-body lib** — desynced collider/mesh, no clean
  blending/merging, re-meshing morphing surfaces is the exact failure of ADR-0012.
- **Full MPM/FEM for everything** — gorgeous and unified, but too costly to be the
  default; kept as an *optional* heavyweight tier for true goo, not the baseline.

## Consequences
- Two new crates and a real research surface. We ship **tiers incrementally**
  (Morph + FieldBlend + basic XPBD soft-body + simple adhesion first; full MPM
  soup, robust topological fracture, and live mesh splitting are later/research).
- A per-material **interaction matrix** (merge/mix/reject + blend radius) becomes a
  first-class authoring concept.
- Full design + math: [`../subsystems/deformable-matter.md`](../subsystems/deformable-matter.md).
