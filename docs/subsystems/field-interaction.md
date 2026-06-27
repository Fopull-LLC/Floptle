# Floptle — Field Interaction (`floptle-rules` data + runtime executor)

A typed graph where "field A modulates field B" is an **authored edge** — the seam that lets gravity, density, light, time, and geometry *affect each other*, turning coincidental composition into designed composition.

> Decision & rationale: [`../decisions/0019-field-interaction-graph.md`](../decisions/0019-field-interaction-graph.md).
> Reads-with: the law-axis spine [`./world-rules.md`](./world-rules.md) (the `floptle-rules` crate, ADR-0018), the matter solver + its interaction matrix [`./deformable-matter.md`](./deformable-matter.md) (ADR-0013), and gravity-as-field [`./gravity-and-density.md`](./gravity-and-density.md) (ADR-0014).

This document designs the **emergence seam**: not a new subsystem, but the wiring between the ones we already have. The graph **data** is pure RON in `floptle-rules`; the **executor** is a runtime system in the fixed-step loop that may form cycles the crate dependency graph deliberately cannot. Build it **thin, with one proof** — the discipline note (§9) is load-bearing, not a footnote.

## 1. The honest gap

Floptle's substrate already unifies **reading**. Render and physics both sample one `f(p, t)`; a deformed wall is renderable *and* collidable with no sync because both consumers read the same field. That is the win ADR-0012/0013 bought us.

But the *gameplay* fields — density, gravity, light, time, temperature — currently **compose only by coincidence**. Every field is a strict **one-way reader**: gravity reads density, matter reads gravity, the renderer reads everyone, and *nothing reads back*. ADR-0014 says it plainly: "`matter` writes density; gravity *reads* it. One-way flow, no cycle." Density never feels the gravity it caused; the molten region never tells the surface its pull just dropped.

That acyclicity is **correct for code**. Cutting the density↔gravity cycle is exactly how `floptle-rules` stays read-only to render/physics/matter and the crates compile. The mistake would be letting that constraint **leak into the simulation layer**, where it forecloses the feedback loops that produce *un-authored surprise* — the difference between "a stack of impressive demos" and "a universe whose surprises you didn't individually author." The "matrix god" promise (ADR-0013) is specifically about defining how forces **interact**; a graph of one-way readers can't keep it.

The trap is timing. Retrofitting feedback into an architecture built on one-way assumptions is nearly impossible — every system that calcifies around "I only read, I'm never read back" becomes a wall. **So the seam must exist before the systems harden**, even if it starts nearly empty. We are not building the full simulation today. We are reserving the shape so it *can* be built without a rewrite.

## 2. The mechanism — a typed field-interaction graph

Promote "field A modulates field B" from an implicit `use` in some crate to a **first-class authored edge** with a gain and a curve. The set of edges is a small directed graph. The executor walks it at **low cadence**, with **explicit damping**, at a **defined quiet point** in the loop. Crucially, the graph is **architecturally distinct** from crate dependencies: it lives as data in `floptle-rules`, and the executor — a runtime system — is free to form cycles the crate graph forbids.

```rust
// floptle-rules — pure data, no engine deps beyond core + field.

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field { Sdf, Density, Gravity, Light, Time, Temperature }

#[derive(Clone)]
pub struct InteractionEdge {
    pub from:  Field,        // the source field, sampled at (p, t)
    pub to:    Field,        // the field it modulates
    pub gain:  f32,          // signed strength; 0.0 disables without deleting
    pub curve: Curve,        // how `from`'s value maps before scaling by gain
    pub clamp: (f32, f32),   // hard bound on the contribution (safety, §6)
}

#[derive(Clone, Copy)]
pub enum Curve { Linear, Smoothstep, Pow(f32), Threshold(f32), Curve(CurveId) }

#[derive(Clone, Default)]
pub struct InteractionGraph { pub edges: Vec<InteractionEdge> }
```

An edge means: *at the quiet point, sample `from` at `p`, push it through `curve`, scale by `gain`, clamp, and accumulate the result into `to`'s modulation buffer for the next step.* `to` is never overwritten — edges **accumulate** into a per-field delta so multiple sources compose additively (order-independent up to the clamp). The base field is authored; edges are forcing terms layered on top.

The three couplings we already have become the **default edges** every world ships with — same behavior as today, now expressed *as data*:

```
density        ──gain──▶ gravity      (more mass → more pull;   ADR-0014 tier 3)
sdf-gradient   ──gain──▶ gravity      (surface pull g ∝ -∇f;    ADR-0014 tier 2)
gravity        ──gain──▶ matter       (the "down" the solver integrates)
```

```ron
// world.ron — the interaction graph alongside the lawset.
InteractionGraph(
    edges: [
        // defaults: today's one-way couplings, now authored.
        ( from: Density,  to: Gravity, gain: 1.0,  curve: Linear,        clamp: (0.0, 4.0) ),
        ( from: Sdf,      to: Gravity, gain: 1.0,  curve: Linear,        clamp: (0.0, 1.0) ),  // gradient pull
        ( from: Gravity,  to: Matter,  gain: 1.0,  curve: Linear,        clamp: (-9.8, 9.8) ),

        // new, the part that was impossible before — feedback edges.
        ( from: Temperature, to: Sdf,     gain: -0.6, curve: Smoothstep,    clamp: (-1.0, 0.0) ),  // heat melts geometry
        ( from: Sdf,         to: Density, gain: 0.5,  curve: Threshold(0.0), clamp: (0.0, 1.0) ),   // soup → low density
    ],
)
```

Nothing in that RON could be expressed with a `use` statement, because `Temperature → Sdf → Density → Gravity → Matter → (heat?)` is a **cycle**. The crate graph cannot represent it; the runtime executor can.

## 3. Generalizing the material-interaction matrix

We already shipped the most emergence-shaped thing in the engine: the **material-interaction matrix** (ADR-0013), a grid of `material × material → {Merge, Mix, Reject, Ignore}` with a blend radius. It is exactly "given two things, how do they combine" — authored as a table, not branched in code.

The field-interaction graph is that idea generalized one axis up: from *"material A blends with material B (geometry only)"* to *"**any** field A modulates **any** field B."* The material matrix becomes a special case — the `Sdf → Sdf` family of edges, keyed by material pair. The graph is the same authoring instinct (a cell you fill in, a gain you dial) applied to the whole field set instead of just geometry. One mental model, two scopes:

```
material-interaction matrix   (ADR-0013)   Sdf × Sdf  → blend rule        (geometry)
field-interaction graph       (ADR-0019)   Field → Field  → gain · curve  (everything)
```

Authors who already understand the blend grid understand the graph for free.

## 4. The rule-lens overlay — legibility

**Emergence without legibility is noise.** If heat silently melts a wall and the player can't see *why* the floor turned to soup, it reads as a bug, not a rule. So the graph ships with a debug visualizer:

- **Rule-lens** — pick any `Field` and render it over the scene: scalar fields (density, temperature, time-rate) as a false-color ramp; vector fields (gravity) as arrows/streamlines. The same machinery the renderer already uses to sample `f(p, t)` drives it.
- **Edge highlight** — select an edge in the node-graph and the lens shows *its contribution* (the post-curve, post-gain delta it pushes into `to`), so you see precisely how much each rule is doing where.

Pair this with **determinism** (already an engine goal, §6). A discovered trick — "heat that pillar and it'll drop you off the side" — is **repeatable**, frame-for-frame, from the same inputs. That is the precondition for *mastery* rather than *luck*: surprise the developer didn't author, but the player can learn, because the universe is lawful. Emergence + legibility + determinism is the whole bet.

## 5. Where it runs — the loop

The executor is a runtime system, but it is **not on the per-step hot path**. It runs at a **defined quiet point between fixed steps** — the same instant the floating-origin rebase runs ([`./large-world-space.md`](./large-world-space.md) §5), so a physics step never spans a graph update and never reads a half-applied field.

```
fixed step N:
  ├─ input → integrate physics (reads gravity field as authored last quiet point)
  ├─ matter solve, morph, particles
  └─ render (aesthetic consumers may read live; see §6)
[QUIET POINT]  ← floating-origin rebase + field-interaction executor
  ├─ for each edge in fixed order:  delta[to] += clamp(gain · curve(sample(from, p)))
  ├─ apply deltas with damping:     field[to] = lerp(field[to], base + delta, k)
  └─ (cadence: every M steps, M ≥ 1; cheap edges can run hotter)
fixed step N+1: ...
```

Low cadence is a feature, not a compromise. Feedback in nature is slow relative to the action; melting a wall over a couple hundred milliseconds *feels* right and costs almost nothing per frame. Cheap edges (the gravity defaults) can run every step; expensive ones (a density-field gravity re-solve) run every M steps and interpolate between.

## 6. Determinism & safety

The graph is power, and power is how you get oscillation, blow-up, and irreproducibility. Four rules keep it tame:

- **Fixed iteration order.** Edges execute in a canonical order (declaration order in the RON, ties broken by `(from, to)`). Accumulation into deltas is additive and order-independent up to the clamp, but we fix the order anyway so the clamp itself is deterministic.
- **Explicit damping.** Every field's update is a `lerp` toward its target with a damping factor `k < 1`, plus per-edge `clamp`. This is what stops a `density → gravity → compaction → density` loop from ringing into chaos. Under-damping is the failure mode; we default conservative and let authors push.
- **Quiet-point iteration.** As in §5 — no consumer reads a field mid-update. The executor reads last step's fields, writes next step's, atomically at the boundary.
- **Authoritative vs. aesthetic split.** Edges that feed the **authoritative** sim (gravity, sdf-collision, density) run in the deterministic executor. Edges that feed only **aesthetic** consumers (light color reacting to temperature, a shader tint) may read live and off the authoritative path — they can never desync the sim because nothing authoritative reads them back.

And the architectural rule that makes all of this possible: **the runtime graph is separate from crate deps.** Conflating them gives you one of two failures — either you try to express the cycle in `use` statements and hit a **compile-time impossibility**, or you flatten the loop into the hot path and get an **under-damped sim** with no quiet point to stabilize it. Keeping the executor as a runtime system over pure data is what buys both the cycle *and* the safety.

## 7. The vision fork (a call for Ty, not a tech default)

How *deep* the graph runs is a vision decision, and the two ends pull against each other:

```
authored-dream ◀─────────────────────────────────▶ emergent-sim
shallow, damped, mostly one-way            full cyclic graph, deep feedback
LOOKS emergent, stays tame/reproducible    richer real surprise
sculpts well, surprises rarely             fights determinism + "cleanly
                                           interactable" promise
```

- **Authored-dream** — a shallow graph of mostly one-way, heavily-damped edges. Things *appear* to react to each other, but the developer is really sculpting a curated dream: tame, reproducible, easy to ship, surprises mostly authored.
- **Emergent-sim** — the full cyclic graph with real feedback at depth. Genuinely surprising, genuinely a *universe* — and genuinely at war with determinism and the promise that the world stays cleanly interactable rather than dissolving into chaos.

**Recommendation: the hybrid.** Author explicit **forcing terms** (designer-placed sources: "this region is hot," "this realm pulls harder") layered over a **shallow damped graph** that fills in responsive detail. The developer sculpts the dream; the rules paint the reactive texture inside it. This keeps the surreal, controllable intent (Floptle is aimed at *surreal*, not realism) while still earning surprise the developer didn't hand-place. But the dial between dream and sim is **Ty's to turn per project** — it is a creative call, not an engine default, and the architecture supports any point on the line.

## 8. Editor UX

- **Field node-graph** — fields as nodes, edges as connections, rendered with the existing **shader-graph widget** style (ADR-0007 / [`./shaders.md`](./shaders.md)). Drag from `Temperature` to `Sdf` to author an edge; the cycle that `floptle-rules` data allows is drawn as a real loop in the canvas — you *see* the feedback the crate graph couldn't have.
- **Per-edge inspector** — `gain` slider, `curve` picker (with a live curve preview), `clamp` bounds. Setting `gain = 0` disables an edge without deleting it, so you can A/B a rule.
- **Rule-lens** (§4) — the field visualizer, toggled per field, plus per-edge contribution highlight. This is the legibility half of the bet; ship it *with* the graph, not after.

## 9. Near-term — the seam + ONE proof

The engine is ~100:1 docs-to-code; this subsystem earns its place with **the seam plus exactly one closed loop**, not a general solver. Everything below already exists — brickmap SDF, `smin` blend, surface gravity, raymarch. You add **edges, not subsystems.**

**The proof: a melting fractal wall.** Three edges, nothing scripted:

```
1.  temperature → sdf      heat flows into a fractal wall; warm bricks lower their
                           SDF toward soup (smin pulls the surface down).
2.  (shared field)         the melted region is *automatically* collidable and
                           walkable — render and physics read the same f, so the
                           hole you melted is a hole you can step through. Free.
3.  density → gravity      molten region's density drops → surface gravity there
                           weakens → you SLIDE off the softened slope.
```

Heat the wall. It melts *exactly where heated* (edge 1). The melt is instantly traversable because there is one field, not three representations (the substrate, edge 2 needs no work). And as it softens, the ground stops holding you and you slide off (edge 3). **Three rules wired as edges produce one coherent surprise** — and you authored none of the surprise, only the three edges. That is the whole thesis in a single demo, built from parts that already ship.

Proof scope is deliberately minimal: the `temperature → sdf` and `density → gravity` edges, the additive delta buffer, the quiet-point executor, conservative damping, and the rule-lens for `temperature` + `density`. No transport, no equilibria.

## 10. Out of scope (research / deferred)

- **A full physical multiphysics solver.** We are wiring authored edges, not coupling PDEs.
- **Conserved transport** — real heat diffusion/advection (heat that *flows* and is *conserved* across the volume rather than applied as a forcing term). Trends toward realism.
- **Realistic equilibria** — sand piles that settle like real sand, fluids that level. These pull hard toward *realism* and fight the surreal intent; they belong to a later research track if ever.
- **Deep cyclic feedback at scale** — the `emergent-sim` end of §7. The seam supports it; we do not build it until the look and the one proof are solid.

The discipline is the point: reserve the shape now (the seam, before systems calcify), prove it once (the melting wall), and defer the heavy transport/equilibria work until the universe has earned it.
