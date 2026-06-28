# ADR-0020 — Fractal as a first-class shape primitive

- **Status:** Accepted · 2026-06-28
- **Decider:** Ty Johnston (Fopull LLC)

## Context
The proof slices keep reaching for fractals by **hand-writing a distance estimator
in WGSL and re-deriving the same support machinery each time** (Mandelbox in Beat
1, a "walkable" macro field in Beat 2, a Mandelbulb then a rounded Menger sponge in
Beat 3). Every beat re-solves the same problems from scratch: which estimator is
*walkable*, how to collide against it, what "down" means, how to descend into it
without precision blowing up. That's exactly the kind of thing the vision says the
**engine** should own — "another dimension" worlds and *infinite fractal depth* are
headline promises, not one-off shader tricks. A maker should be able to drop a
**fractal** into a scene the way they drop a sphere or a box, and have render,
collision, gravity, and large-world descent **just work**.

Beat 3 also produced hard-won, measured knowledge that must not be lost:
- **Not every fractal is playable.** Measured: the raw Mandelbox has an *empty
  interior* (nothing to stand on) and ~86–179° normal flips per step → unwalkable.
  The Mandelbulb is solid and walkable (~10°/step, `|∇f|≈0.8`, ~11% solid) but
  *bounded and non-porous* — "just a bumpy planet" you skirt the edge of. The
  **rounded Menger sponge** is the one you can go *inside*: ~88% open, a lattice of
  tunnels with walls you stand on (~17°/step, `|∇f|≈0.71`).
- **Render-detailed / collide-smooth.** The eye can take sharp fractal crust; the
  feet need a tamer field. The two reads of `f(p,t)` can differ (an iteration cap /
  rounding for collision, full detail for render) — same shape, two LODs.
- **"Down" is `-∇f`.** Gravity toward the nearest wall (the local distance gradient)
  lets you walk tunnel floors and run up walls without a special-cased core.
- **Infinite descent = shrink the observer, not rebase the field.** The player
  scale `s = k^(-dive)` shrinks as you dive while the estimator gains iterations;
  sub-tunnels open up around you with *no pop*. This is ADR-0015 (large-world /
  floating origin) applied at the *fractal* scale — the same "world moves around a
  near-origin player" idea, recursively inward.

The discipline that made Beat 3 work — **measure the field in a scratch project
before wiring physics to it** (frac-solid, normal-rotation/step, `|∇f|`,
self-similarity factor) — is itself a feature the engine should provide.

## Decision
Make **`Fractal` a first-class SDF shape** in the same authorable set as
`Sphere`/`Box`/`Plane`, evaluated through the one `f(p,t)` substrate that render
**and** physics already share (ADR-0012). A fractal node carries:

1. **A named estimator** from a built-in library (`menger`, `mandelbulb`,
   `mandelbox`, `sierpinski`, …) selected as an enum/param, **plus a `Custom`
   escape hatch** (a shader-IR expression, per ADR-0007) so makers aren't limited
   to the presets. Each built-in is **tagged with measured playability metadata**
   (solid-fraction, normal-rotation/step, `|∇f|`, self-similarity factor, "porous?"
   "walkable?" "delvable?") so the editor can warn "this one is great to look at,
   terrible to walk on" instead of letting a maker discover it the hard way.
2. **Per-shape params** with sane defaults: iteration count (with a separate
   **collision iteration cap** for the render-detailed/collide-smooth split),
   rounding radius (`smin`/`smax` to make boxy fractals organic), scale, and a
   morph hook (any param can be driven by a field / `time`, per ADR-0017).
3. **Descent built in.** A fractal can be flagged **delvable**; the engine then
   provides the **shrink-the-observer infinite-zoom** (player scale + iteration
   unfold) as an out-of-the-box behavior wired to large-world space (ADR-0015),
   not something each game re-implements. The estimator's **self-similarity factor**
   is metadata so the engine scales/recenters correctly (Menger is factor 3, not 2).
4. **Collision + gravity for free** — depenetration against `f`, `-∇f` "down", and
   the analytic **surface-velocity carry** (`df/dt` of the morph, so a rising wall
   lifts the player) come from the shared substrate, configurable per shape.

A **"fractal lab"** editor tool runs the Beat-3 measurement pass (the scratch
`fieldcheck`, productized) on any estimator + params and shows the playability
metadata live, so "is this walkable / delvable?" is answered *before* you build a
level on it.

## Why
- **We own the renderer and the physics** (ADR-0001/0002/0012), so a fractal can be
  one shape read two ways instead of a bolt-on. Bolt-on engines can't do this.
- It turns repeated proof-slice pain into a **reusable primitive** — the whole point
  of dogfooding the proof slices is to discover exactly this surface.
- Composes with the rest of the substrate: morph via **time-as-a-field** (ADR-0017),
  descent via **large-world space** (ADR-0015), blends with other SDFs via the
  **field-interaction graph** (ADR-0019), authored via the **shader IR** (ADR-0007).
- **Proven in miniature:** `floptle-proof --bin descent` already does all of this by
  hand (rounded Menger, `-∇f` gravity, shrink-and-walk-in descent, render/collide
  split). This ADR is the commitment to lift it out of the one-off binary.

## Alternatives considered
- **Keep fractals as hand-written WGSL per game.** The status quo of the proof
  slices; re-derives playability + collision + descent every time. Rejected as the
  long-term answer (fine for *proofs*, which are dead-ends by design).
- **A generic "custom SDF" node only (no named library, no metadata).** Maximally
  flexible but hands the maker every sharp edge (unwalkable estimators, precision
  blowups) with no guardrails — the opposite of "handle this out of the box."
- **Bake fractals to meshes/voxels at author time.** Throws away infinite depth and
  cheap morph — the entire reason to use a fractal here.

## Consequences
- The shape/primitive enum and the shader-IR shape set both gain a `Fractal` variant
  with an estimator library; the IR must express the named estimators + `smin/smax`.
- The estimator library needs **playability metadata** captured once (the `fieldcheck`
  measurement pass, productized as the "fractal lab" tool).
- Descent/large-world (ADR-0015) gains a **per-shape "delvable" path** (shrink-the-
  observer + iteration unfold + self-similarity-aware recenter).
- This is a **later-phase** feature (it presumes the shape system, shader IR, and
  large-world space exist); near-term it stays hand-written in `floptle-proof`. The
  proof binary is the reference implementation a future subsystem doc grows from.
