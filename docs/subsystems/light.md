# Floptle — Programmable Light (`floptle-render` + shader-IR light nodes)

Light is not a fixed renderer feature — it is a composable **transport rule**
sampled along the rays the marcher already shoots, the fourth peer field
(space · matter · gravity · **light**), off by default and bit-identical when unused.

> Decision & rationale: [`../decisions/0016-programmable-light.md`](../decisions/0016-programmable-light.md).
> Reads-with: the raymarch loop & post stack [`./renderer.md`](./renderer.md) (§3 is the host),
> the shader IR [`./shaders.md`](./shaders.md) (light-rules are additive stdlib nodes),
> and the gravity field [`./gravity-and-density.md`](./gravity-and-density.md) (whose `g(p)` we reuse verbatim).

## 1. Core principle — the renderer already *is* light transport

The headline loop in [`./renderer.md`](./renderer.md) §3 sphere-marches `f(p,t)`
along a ray. That loop **is** light transport — with the transport rule hardcoded
to *"straight line, vacuum, no absorption."* Make that rule a first-class,
developer-authored object and light joins the thesis: geometry, gravity, and light
become the same kind of thing — a field sampled at `(p,t)` — so they **compose**.

```
today:   p = ro + rd·t       // rd is CONSTANT — a straight ray, the rule baked in
ours:    p = ro + rd·t,  rd = rule(p, rd, t)   // the rule is now data the dev owns
```

Light is **tiered like gravity** (ADR-0014): cheapest → heaviest, each tier opt-in,
a tier never drags in the machinery of the tiers above it. **Tier 0 is the current
renderer, unchanged** — zero light cost, zero bend, no regression. You pay only for
the tier you reach for.

```
cost  ▁▁▂▂▃▃▅▅▇▇█
      Conventional   Radiance hook   Bent transport   Participating media
        │              │                │                 └ signed σ_e/σ_a along the march (research)
        │              │                └ rd = normalize(rd + bend(p,rd,t)·d) — THE HEADLINE, nearly free
        │              └ replace L(p,n,view,λ,t) per surface — non-physical, field-driven "mood"
        └ straight rays + SDF soft-shadow/AO — today's renderer, FREE
```

## 2. Tier 0 — Conventional (the free default)

Straight rays, SDF soft shadows, SDF ambient occlusion — exactly what
[`./renderer.md`](./renderer.md) §3 already ships (smallest `d/t` penumbra march
toward the light; AO from a few `map()` taps along the normal). No new data, no new
pass. **When every light tier is off the compiled WGSL is identical** to today's —
that's the contract, asserted bit-for-bit by the proof harness. Cost: zero over baseline.

## 3. Tier 1 — Programmable radiance hook

Promote the raymarch **shade hook** (`./shaders.md` §4.1's optional shade/normal
hooks on a `Raymarch` node) to a full **lighting rule**:

```
L(p, n, view, λ, t) -> Color
```

The developer *replaces the lighting equation per surface*. Lighting need not be
`albedo · light` — it can be driven by an **authored scalar field sampled exactly
like density** ("mood," "dream-coherence," "memory"), so a surface glows by how
*haunted* a point is, not how lit. The field is read from `floptle-field` the same
way gravity reads `ρ(p)` — one substrate, many meanings.

```flsl
// A light-RULE node, additive to the shader-IR stdlib (ADR-0007).
shader dream_shade {
  stage raymarch
  uniform time: float

  // hook signature the marcher calls at the hit point:
  light L(p: vec3, n: vec3, view: vec3, lambda: float, t: float) -> color {
    let mood   = field_sample(p, "dream_coherence");   // authored scalar, like density
    let rim    = pow(1.0 - max(dot(n, view), 0.0), 3.0);
    let glow   = palette(mood + rim, "bruise");        // non-physical color
    output color = glow * (0.4 + 0.6 * mood);          // lit by MEANING, not photons
  }
}
```

It transpiles to a WGSL function the raymarch hit-path calls instead of the default
Lambert/rim. No transport change yet — rays are still straight — so it stays as cheap
as Tier 0 plus whatever the rule samples (one rule eval per hit). *Use when:* stylized
surfaces, field-driven "lighting," palettes detached from optics.

## 4. Tier 2 — Bent transport (the headline, nearly free)

The rays the marcher *already shoots* get a per-step **direction operator**. In the
existing loop, carry `dir` as a variable and bend it each step by a programmable
`Vec3` field:

```wgsl
var dir = rd;                          // primary ray direction, now MUTABLE
var t   = near;
var pos = rd * near;                    // position offset from ro along the path
for (var i = 0u; i < MAX_STEPS; i++) {
    let p = ro + /* path-integrated */ pos;
    let d = map(p, time);
    if (d < EPS * t) { hit = true; break; }
    dir = normalize(dir + bend(p, dir, time) * d);  // <-- the whole feature
    pos += dir * d * STEP_RELAX;                    // step ALONG the (curved) path
    t   += d * STEP_RELAX;
    if (t > far) { break; }
}
```

`bend(p, dir, t)` is a **programmable field** (a light-rule IR node). The headline
property: **`bend == 0` is bit-identical to Tier 0** — same steps, same hits, no cost
regression — so this can ship dark and turn on per-scene.

**What `bend` buys you, by what you plug in:**

```
bend = -k · grad(f)     rays curve TOWARD surfaces       → caustic gather, light pooling in folds
bend =  g(p)            PHOTONS FALL under your gravity   → lensing / black-hole ring, NO new data
bend =  warp(p,t)       authored swirl/vortex            → impossible refraction, dream optics
```

The second is the punchline: `g(p)` from
[`./gravity-and-density.md`](./gravity-and-density.md) **already exists** and is
already sampled cheaply. Feed it as `bend` and light bends in the same well that
pulls the player down — *one authored rule driving two unrelated phenomena because
they are the same kind of object.* You get a gravitational-lensing / black-hole-ring
look with **no new data structure** — the field is shared.

**Impossible shadows — apply the SAME bent march to the shadow ray.** The Tier-0
shadow march toward the light becomes a *bent* march. A shadow ray that curves around
a corner casts a shadow **detached from its caster** — a shadow that bends, runs
ahead of you, or falls where no occluder sits:

```
   light ☼                          straight shadow ray:  ── occluded? ──▶ caster shadows under it
     ╲                              bent shadow ray:       ╲__ curves around the pillar →
   ┌──┐  pillar                                               shadow appears where NOTHING blocks
   │  │            ●  the shadow lands here, detached from the pillar that "cast" it
   └──┘        ╱
            ___╱  (bent path)
```

**The math note.** This is a discretized **ray ODE** — eikonal / geodesic
integration, `d(dir)/ds = bend`. With `bend = g(p)` it is literally photons on a
trajectory under an acceleration; with a metric it'd be a geodesic. We use a
**developer-authored acceleration term instead of a metric** — same shape, far
cheaper, bending to *your* rules, not Einstein's. First-order (forward-Euler on
direction) is enough for the look; the step is the SDF distance the march already
took, so no extra sampling beyond the `bend` eval. *Use when:* lensing, caustic
pooling, impossible/running shadows. Cost: one `bend` eval/step + worse early-out (§10).

## 5. Tier 3 — Participating media (research)

Accumulate radiance along the march instead of only at the hit:

```
L += sigma_e(p) - sigma_a(p) · L        // emission gained, absorption removed, per step
```

The twist that makes it *Floptle* rather than fog: the coefficients are **signed**.
`sigma_e < 0` is "dark that emits" — a region that *removes* light it passes; a
**negative shadow** that brightens what's behind it. Wonderful, and **dangerous**:
signed accumulation is **order-dependent** (front-to-back ≠ back-to-front) and
**NaN-prone** (a negative `sigma_a` can blow `L` up unboundedly). Flagged research —
it ships behind a clamp, a step cap, and a "this can explode" warning, after Tier 2
is solid. The heaviest tier by far; real per-step accumulation.

## 6. Light as an *emitted* field

Tiers above govern *transport*. The other half is **emission**: light is also a
**field a material radiates**, stored on `floptle-field`'s scalar layer — the **same
brickmap channels ADR-0014 added for `ρ`/`Φ`/`g`**, now carrying per-material
radiance and absorption. No new storage: another channel on bricks that already exist.

- **Signed + spectrally weird.** Radiance may be negative (a material that *darkens*)
  and band-dependent. Authoring stays per-material, beside the density/matter constants.
- **Composes with CSG.** Emission is a field, so `smin`/`smax` blend it: melt two
  glowing soups and **their light blends** with their geometry — one `smooth_min` does
  both, no separate light-merge step.
- **Gravity can MODULATE emission.** `emit(p) = saturate(density(p))` makes dense
  cores glow — **you literally *see* mass**. Same substrate, so coupling is a multiply.
- **Cheap dispersion.** A 3–4-tap "wavelength" loop (march R/G/B with slightly
  different `bend`/absorption) gives **prisms into impossible palettes** at ~3× a
  band's cost — opt-in. A material can be **opaque to one band and invisible to "dream
  light,"** because absorption is per-band per-material.

```
   two glowing fractal soups          smooth_min blends GEOMETRY *and* EMISSION:
    ◐ red glow   ◑ blue glow    ──▶   ◑◐  one body, light is the blend, edges bleed
```

## 7. Field-coupling presets — the demo reel that proves "rules compose"

Each preset is a **RON file** plus a one-line Lua call (mirrors `gravity.set_mode`,
[`./gravity-and-density.md`](./gravity-and-density.md) §7). They exist to *show*, in
one line, that light is a field that couples to the other fields:

```lua
light.set_mode(scene, "falls_under_gravity",  { strength = 1.0 })  -- bend = g(p)
light.set_mode(scene, "gravity_glows",        { gain = 1.0 })      -- emit = saturate(density)
light.set_mode(scene, "shadow_runs_with_you", { strength = 6.0 })  -- shadow-ray bend = player-relative field
light.set_mode(scene, "surface_paints_light", { })                 -- tint from the marcher's orbit-trap
light.set_mode(scene, "time_curves_light",    { strength = 2.0 })  -- bend = ∂f/∂t (morph velocity)
```

| preset | what it couples | the wiring |
|---|---|---|
| `light_falls`         | light ← gravity   | `bend = g(p)` — photons fall, lensing, ring |
| `gravity_glows`       | light ← density   | `emit = saturate(density(p))` — see mass |
| `shadow_runs_with_you`| shadow ← player   | shadow-ray `bend` = field around the player |
| `surface_paints_light`| light ← geometry  | tint from the **SDF orbit-trap value the marcher already computes** |
| `time_curves_light`   | light ← time      | `bend = ∂f/∂t` — the morph-velocity the physics already uses |

```ron
// "Light falls into your gravity, and dense cores glow." Two couplings, no new data.
Light(
    transport: Bent(BendField(           // Tier 2
        source:   Gravity,               // bend = g(p) from floptle-physics::gravity
        strength: 1.0,
        max_steps: 96,                   // §8 cap for bent rays
    )),
    shadow:    Bent(strength: 1.0),      // bent shadow rays → impossible shadows
    emission:  Some(EmitField(           // §6, on the shared brickmap channel
        source: Density,                 // emit ∝ density → "you see mass"
        gain:   1.0,
        signed: false,
    )),
    media:     None,                     // Tier 3 off (research)
)
```

Note `source: Gravity` and `source: Density` read the **same fields** gravity and
matter already populate — the preset is a *wiring*, not new simulation.

## 8. Composition

Light is the **compose of enabled tiers**, the exact shape gravity uses
(`g = Σ tiers`, [`./gravity-and-density.md`](./gravity-and-density.md) §3):

```
bend(p,dir,t) = Σ enabled bend tiers        // grad-pull + g(p) + authored warp + ∂f/∂t
L(hit)        = radiance_rule  ⊕  Σ emission(p) along path  ⊕  media accumulation
```

Each term is opt-in and independently weighted; absent terms cost nothing and emit
nothing. Bend fields **sum** (a vortex *and* gravity); emission fields **blend** under
CSG (§6).

```rust
/// What a camera/scene asks of light. Mirrors GravityField (gravity-and-density §3).
pub struct LightRules {
    pub radiance: Option<RadianceRule>,   // Tier 1: L(p,n,view,λ,t)
    pub bend:     Vec<BendField>,         // Tier 2: summed direction operators
    pub shadow:   ShadowMode,             // Straight | Bent — bent shadow rays
    pub emission: Option<EmitField>,      // §6: per-material radiance on the brickmap
    pub media:    Option<MediaField>,     // Tier 3: signed σ_e/σ_a (research)
    pub spectral: SpectralMode,           // Mono | Dispersed(taps)  — §6 dispersion
}
// Default: { radiance:None, bend:[], shadow:Straight, emission:None, media:None, spectral:Mono }
//          == Tier 0 == today's renderer, bit-identical.
```

## 9. Editor UX

Same lever as every other Floptle visual — the shader IR ([`./shaders.md`](./shaders.md)):

- **Light-rule nodes** live in the shader graph as a new stdlib category (`light.bend`,
  `light.radiance`, `light.emit`, `light.media`), wired with typed ports like any node.
- **Open in VSCode** ([ADR-0011](../decisions/0011-vscode-integration.md)) prints the
  rule to `.flsl`; edit by hand or with AI; save re-syncs the graph. A `bend` field is
  just a `Vec3`-returning subgraph — author it like any SDF warp.
- **Live preview** recompiles on edit; flip a `light.set_mode` preset in the inspector
  and the bent rays / glow update immediately. naga errors map back to the node / line.

## 10. Performance posture

Hyperoptimization is the requirement (ARCHITECTURE §9); the tier ladder *is* the
optimization. **Be honest about which tiers are free:**

- **Tier 0/1 are free** (zero / one-rule-per-hit). **Tier 2/3 are NOT free.**
- **Bent secondary rays cost more and early-out less cleanly.** A straight ray escapes
  the bounds and stops; a curved ray can re-enter, so the far-plane early-out fires
  later → hard **`MAX_STEPS` cap on bent passes** (the RON `max_steps`), separate from
  the primary cap.
- **Half-res + upscale** for bent/media passes (the renderer's existing trick,
  [`./renderer.md`](./renderer.md) §6) — bent rays are low-frequency, they upscale well.
- **The step-count heatmap profiler** ([`./renderer.md`](./renderer.md) §6) is the
  guardrail: turn on `bend` and *watch* which pixels burn budget. We measure, we don't
  assume — bent rays are exactly where the heatmap earns its keep.
- **Dispersion is opt-in** and multiplies band cost (§6); default `Mono`.

## 11. Out of scope / honesty

The [`./renderer.md`](./renderer.md) §7 fence holds — this is **stylization, not
realism**:

- **Not rebuilding Lumen**, not a PBR path-tracer, not GI-for-realism. We bend light
  for *strangeness*, not for accurate bounce.
- **Stay single-march.** No second geometry model, no acceleration structure for
  light, no light-baking. Everything rides the one sphere-march and the shared field.
- Light is **opt-in per game**: most games never leave Tier 0 and never think about
  any of this. The engine makes it *easy* when wanted (because it already understands
  the field) and *absent* when not.

If a light feature serves correctness over wonder, it doesn't belong here.

## 12. Near-term vs research (the thin seam, smallest proof first)

This engine is documentation-heavy; light ships as a **thin seam, smallest proof
first, off by default — never a big-bang that delays pixels.** The whole of Tier 2
is *one mutable variable and one line* in a loop that already exists.

**The proof (near-term, first):**

1. In the **hardcoded-WGSL raymarch proof** (not the IR yet), make `dir` mutable and
   add the one bend line — guarded so `bend == 0` is asserted **bit-identical** to the
   current frame (the no-regression contract).
2. Plug `bend = -grad(f)` (reuses the normal gradient already computed) → rays pool
   toward surfaces. *First visible win.*
3. Plug `bend = g(p)` (reuses the gravity field already sampled) → **photons fall**,
   the lensing/ring look, with **no new data**. *The headline, proven cheap.*
4. Apply the same bent march to the **shadow ray** → one impossible shadow. *Proof the
   primitive generalizes.*

That is the entire near-term commitment: ~10 lines in one shader, off by default.

**Deferred / research — flagged, not promised:**

- Tier 1 radiance-rule and Tier 2 bend promoted to **shader-IR stdlib nodes** + the
  `.flsl` `light` hook syntax (§3, §9) — additive, after the WGSL proof.
- **Emission as a brickmap channel** (§6) and CSG-blended light — needs the field layer
  to carry the extra channel.
- **Field-coupling presets** + `light.set_mode` Lua surface (§7) — sugar over the proven
  primitives. **Dispersion / spectral** taps (§6) — opt-in, after mono is solid.
- **Tier 3 participating media** with signed coefficients (§5) — genuinely hard
  (order-dependent, NaN-prone); the long arc, like the gravity Poisson tier.

Everything past step 4 waits. The renderer keeps shipping pixels the entire time
because every light tier defaults to *the renderer we already have*.
