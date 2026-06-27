# Floptle — Deformable Matter (`floptle-matter`)

Make geometry a *physical material* — morphing, blending, sticky, breakable — that stays renderable and collidable for free because it is always the same implicit field.

> Decision & rationale: [`../decisions/0013-deformable-matter.md`](../decisions/0013-deformable-matter.md), built on SDF-first physics [`../decisions/0012-physics-sdf-first.md`](../decisions/0012-physics-sdf-first.md).
> Reads-with: the SDF collision core [`./physics.md`](./physics.md), the raymarch/mesh renderer [`./renderer.md`](./renderer.md), and the shader IR [`./shaders.md`](./shaders.md).

`floptle-matter` depends on `floptle-core` (math · ECS · node facade · time),
`floptle-field` (the implicit-field substrate: SDFs, CSG `smin`/`smax`, per-pair
blend rules, mesh↔SDF conversion), and `floptle-physics` (collision queries). It
adds *no* new geometry representation — it only decides **how a field changes over
time** and writes the result back into the shared field that render and physics
already read.

## 1. One substrate, one idea

The engine's whole geometry stack is an **implicit field** `f(p, t)` (a signed
distance, negative inside). Fractals are fields; scene primitives are fields;
imported meshes become fields via a winding-number bake. The renderer raymarches
`f`; physics collides against `f` (ADR-0012). That shared `f` is the load-bearing
trick of this entire document:

> **If deformation only ever edits the field, then a morphing, blending, soft, or
> sticky object is automatically rendered *and* collidable with zero sync.** No
> re-meshing, no second representation to keep in step.

So "everything is changeable, controllable, movable" becomes a concrete contract:
matter is a function of time that writes into `f`. A new `MatterModel` component
declares *how* an object behaves as matter — and nothing more than it needs.

```rust
struct MatterModel {
    tier:   MatterTier,          // Rigid | Morph | FieldBlend | SoftBody | Visco
    blend:  BlendRef,            // material id → looked up in the interaction matrix
    bounds: Aabb,                // conservative; grows with deformation (broadphase)
    sleep:  SleepState,          // Awake | Sleeping(since) — only sim what's touched/seen
}

enum MatterTier {
    Rigid,                                   // tier 0 — no deformation
    Morph(MorphDesc),                        // tier 1 — field displacement (GPU)
    FieldBlend(BlendDesc),                   // tier 2 — CSG smin/smax between fields
    SoftBody(XpbdCage),                      // tier 3 — XPBD particle cage drives mesh
    Visco(ViscoDesc),                        // tier 4 — adhesion/cohesion/fracture
}
```

**Opt-in complexity, mirrored from the engine's philosophy.** An object uses the
*cheapest* tier that gets the look. Rigid is free; Morph is ~free on the GPU;
FieldBlend is cheap; SoftBody is moderate; Visco is advanced. You pay only for
the tier you reach for — and a tier never drags in the machinery of the tiers
above it.

```
cost  ▁▁▂▂▃▃▅▅▇▇█
      Rigid  Morph  FieldBlend  SoftBody  Visco
       │       │        │          │         └ sticky / stretch / break (partly future)
       │       │        │          └ particle cage, skinned mesh
       │       │        └ smin/smax "soup" & carve, per-pair rules
       │       └ p' = p + d(p,t), baked SDF for collision
       └ static field
```

## 2. The deformation tiers

### Tier 0 — Rigid

No deformation. The field is the object's static SDF (or mesh-baked field) under a
rigid transform. This is the floor; most props live here. Listed so the tiers form
one ladder: even rigid bodies expose the same `MatterModel` seam, so any of them
can be promoted to soft/sticky at runtime by swapping `tier`.

### Tier 1 — Morph (≈ free, GPU)

A **displacement field** warps space before the SDF is evaluated:

```
p' = p + d(p, t)            f_morph(p, t) = f_rest(p')      // displaced lookup
```

`d` is built from noise / animation curves / cheap field functions —
breathing surfaces, drifting fractal detail, shifting patterns. On the render side
this is exactly the renderer's vertex/compute displacement (`renderer.md` §4) or a
domain warp in the raymarch `map()` (`shaders.md` §4). It is a pure function of
`(p, t)`: no state, trivially parallel, essentially free per pixel/vertex.

**Collision.** A warp is not an isometry, so `f_morph` is no longer 1-Lipschitz
and sphere-marching can overshoot. For physics we therefore **bake** the displaced
shape into the sparse distance field on a rolling budget and collide against that
baked brick grid (`physics.md` §"Baked sparse SDF") — bake often near bodies, lag a
frame or two far away. Cost stays off the analytic path.

> *Use when:* you want motion in the surface itself — wobble, ripple, melt, flow —
> without simulation. The default upgrade from Rigid.

### Tier 2 — Field blend / CSG (cheap, GPU)

Combine two objects' fields with smooth CSG. The headline operator is the
**polynomial smooth-min** (merge = "soup"):

```
smin(a, b, k) = min(a, b) - h*h*k * (1/4),   h = max(k - |a - b|, 0) / k
```

Smooth-max for **reject / carve** is `smax(a,b,k) = -smin(-a,-b,k)`; hard union is
plain `min(a,b)`. The **blend radius `k`** is the entire dial: `k → 0` is a crisp
boolean seam; larger `k` is soupier — surfaces reach toward each other and fuse
over a band of width ~`k`. Ramp `k` from 0 upward on a curve and one object
*melts into* another in real time.

```
   k = 0 (hard union)        k large (soupy smin)
     ___    ___                ___   ___
    /   \  /   \              /   \_/   \      ← surfaces bridge across the gap
   |  A  ||  B  |            |   A     B  |       over a band of width ~k
    \___/  \___/              \_________/
```

**Which pairs blend is data, not code.** A **material-interaction matrix** maps
each ordered material pair to a rule:

```rust
enum PairRule {
    Ignore,                 // fields don't interact (each rendered/collided alone)
    Merge   { k: f32 },     // smin — soup; k = soupiness
    Carve   { k: f32 },     // smax — B subtracts from A (reject / bite out)
    Hard,                   // boolean min, crisp seam
}
struct InteractionMatrix { rules: HashMap<(MatId, MatId), PairRule> }
```

This is the literal expression of "geometry blends cleanly / mixes / rejects."
The marquee case — **blend an object into a fractal like soup** — is a `Merge`
rule between the object's and the fractal's materials, `k` curve-driven up over a
couple of seconds. Both fields are already in the shared `f`, so the blend happens
inside the field combinator and the result raymarches and collides with no extra
plumbing.

> *Use when:* two surfaces should fuse, bite into, or reject each other. Cost is a
> couple of extra field evals at the overlap — cheap because brickmap sparsity
> means non-overlapping regions never run the combinator.

### Tier 3 — Soft body (moderate)

A particle/constraint **cage** simulated with **XPBD** (Extended Position-Based
Dynamics) drives the visible surface. Per fixed step, for each compliant
constraint with value `C(x)` and **compliance** `α = 1/stiffness`:

```
α̃   = α / Δt²                                  // time-scaled compliance
Δλ  = ( -C(x) - α̃ · λ ) / ( Σ w_i |∇_i C|² + α̃ )
Δx_i = w_i · ∇_i C(x) · Δλ ,    λ += Δλ        // w_i = inverse mass
```

`α = 0` recovers a hard (PBD) constraint; larger `α` is softer. The cage uses three
constraint families:

- **Distance** — `C = |x_i − x_j| − ℓ₀`, the springy skeleton.
- **Volume** — `C = V(tet) − V₀`, resists collapse so the body keeps mass/puffiness.
- **Shape matching** — fit the best rigid transform `(R, c)` to a cluster's rest
  shape and pull particles toward `R·(x₀ − c₀) + c`; gives goo a memory of form so
  it relaxes back instead of puddling.

**Collision is the cheap SDF query.** Each particle is a point (or small sphere);
after the constraint pass we resolve it against the world field with
`world.closest(p, t)` / `sphere_overlap` (`physics.md`): if `f(x) < r`, push out by
`(r − f(x))·∇f`. No contact manifolds, no narrowphase pairs — the field *is* the
collider, so a soft body collides against fractals, primitives, and other matter
through one call.

**Mesh follows the cage** two ways: *skin* the render mesh to the nearest cluster
(linear blend / shape-match transform), or *re-derive* the surface from particles
(Tier 4's reconstruction). Tier 3 defaults to skinning — cheaper, stable.

> *Use when:* something should jiggle, squash, deform and recover — a blob, a plush
> prop, soft terrain. Cost scales with particle count; budgeted and slept (§4).
> "Give an object soft-body physics" is literally `tier = SoftBody`.

### Tier 4 — Viscoelastic / sticky / fracture (advanced, partly future)

Bonds *between* matter surfaces, plus the ability to stretch and break them.

**Adhesion / cohesion = temporary distance constraints.** When two matter surfaces
come within a **contact band** `δ`, spawn a bond between the nearest particles/
surface points — a distance constraint with a rest length and its own compliance:

```
on contact (f_A↔f_B within δ):  create bond (i, j), ℓ₀ = |x_i − x_j|, α_bond
each step:    solve as a distance constraint (XPBD above)
```

A mesh that touches sticky matter gets bonded and **stuck**. Pull away and the bond
elongates; the geometry **physically stretches** because the constraint resists,
dragging the surface (and, via skinning/reconstruction, the rendered field) with
it. This much is **near-term** — it is just XPBD bonds layered on Tier 3.

**Break = strain/force threshold.** A bond is severed when it is overstretched:

```
strain  ε = (|x_i − x_j| − ℓ₀) / ℓ₀
if ε > ε_break  (or bond force > F_break):   remove bond   // snap
```

**Stretch-to-strands** uses **elastoplasticity + damage**: stretch past a *yield*
strain `ε_yield` and the rest length permanently lengthens (plastic flow), so the
matter thins instead of springing back; push to `ε_break` and it tears. Surviving
over-yielded bonds are rendered as **thin tubes / strips** along the bond — the
"stringy lines of mesh pulled too hard." **Splitting** a body in two means
severing enough bonds that the constraint graph falls into disconnected
components; each component becomes its own `MatterModel`, and (optionally) the
surface is **remeshed** from the new particle sets.

> **Honesty.** Near-term: sticky bonds, stretch, single-bond snap. **Research-grade
> future:** robust topological *fracture into strands*, real-time *re-splitting* of
> a body into independent objects, and stable strand remeshing. We ship the bonds
> first and earn the fracture.

## 3. The real math & techniques (so you can research them)

Everything above maps to named, published techniques — none of it is invented here:

- **Signed distance fields & CSG** — `min`/`max` booleans; the substrate itself.
- **Smooth-min** — Quílez's polynomial and exponential `smin`; `k` = blend radius.
- **Mesh → SDF** — *generalized / fast winding number* (Barill et al.) for robust
  inside/outside on imperfect meshes, sampled into the field (`floptle-field`).
- **Field → mesh** — *surface nets*, *dual contouring*, *marching cubes* for the
  far-field/editor mesh when triangles are wanted.
- **XPBD** — Macklin et al., *Extended Position-Based Dynamics*; compliant
  constraints with `α = 1/stiffness`.
- **Shape matching** — Müller et al., meshless deformation; the "memory of form."
- **MPM (MLS-MPM / APIC)** — the heavyweight *unified* solver for true goo/sand/
  snow/viscoelastic "soup"; Disney's *Frozen* snow is the canonical MPM result.
- **SPH + anisotropic-kernel surface reconstruction** (Yu & Turk) — turn a particle
  soup back into a smooth, renderable **and collidable** field.
- **Elastoplasticity + damage** — yield/return-mapping and damage thresholds for
  fracture and the stretch-to-strands behavior.

**Precedent that this ships and looks great:** Media Molecule's *Dreams* (SDF
sculpt + sim in one substrate), *Claybook* (deformable SDF clay you roll and
squish, with physics), and Disney's MPM snow. We are squarely in proven territory —
the novelty is *unifying* these behind one `MatterModel` over a shared field.

## 4. How it all ties together

The **shared sparse field (brickmap)** is the bridge. Deformable objects write
their current field into the same grid the renderer raymarches and physics collides
against. One write, two readers, no sync:

```
   MatterModel.tier ─▶ solver (per tier) ─▶ writes f into shared brickmap
        │                  │                          │
   Morph: p'=p+d      FieldBlend: smin/smax     ┌─────┴─────┐
   SoftBody: XPBD     Visco: bonds+damage        ▼           ▼
   (particles→field)                         RENDERER     PHYSICS
                                            raymarch /    closest /
                                            mesh f        sphere_overlap on f
```

- **Tier 1/2** edit `f` analytically/combinatorially; the brickmap re-bakes the
  touched bricks on a budget (`physics.md`).
- **Tier 3/4** carry particles; their surface is splatted into the brickmap via
  SPH/anisotropic reconstruction so the soft/sticky body is *itself* a field
  others can blend with and collide against — closing the loop (a soft body can be
  `Merge`-blended into a fractal because it, too, is just `f`).

The brickmap is **sparse**: empty bricks are stored once and cost nothing, so a
small deforming region in a huge world is cheap. This is the same brick grid
ADR-0012 already specified — `floptle-matter` is a *writer* to it, not a new system.

## 5. Performance posture

Hyperoptimization is the requirement (ARCHITECTURE §9), not a nice-to-have.

- **GPU compute everywhere it pays** — Morph displacement, CSG blends, and the
  XPBD constraint sweeps run as wgpu compute dispatches; the CPU only orchestrates.
- **Budgets + sleep/wake** — `SleepState` means only matter that is *interacted
  with or on-screen* simulates. A body with no recent contact and near-zero
  particle velocity sleeps; a contact or a script call wakes it. Per-frame caps on
  particles solved, bricks re-baked, and bonds created keep cost bounded.
- **Near/far LOD** — analytic field for matter near the player (exact contact where
  it matters); baked brick field far away; Tier-3/4 bodies drop particle count and
  raise re-bake interval with distance.
- **Fixed timestep + determinism** — all solvers run in the fixed-step stage
  (`physics.md` §"Stepping"). Deterministic constraint/particle ordering keeps the
  sim reproducible — good game-feel and a head start on the future networking goal.
- **SoA particle layout** — positions, prev-positions, inverse masses, and λ in
  parallel arrays for coalesced GPU access and SIMD-friendly CPU fallback.
- **Brickmap sparsity** — non-overlapping, far, and empty space all cost ~nothing;
  the combinator and re-bake only run where matter actually is.

## 6. Authoring & scripting

**MatterModel inspector** — pick the tier; below it, only that tier's knobs appear
(stiffness/compliance and volume preservation for SoftBody; `k` for FieldBlend;
stickiness strength + break strain for Visco). Promoting a Rigid prop to SoftBody
is one dropdown.

**Interaction-matrix editor** — a grid of materials × materials; each cell is a
`PairRule` (Ignore / Merge·k / Carve·k / Hard). This *is* the "which materials
merge, mix, or reject" control surface.

**Lua API** (`floptle-script`, ARCHITECTURE §6) to change matter at runtime —
the "everything is controllable" promise:

```lua
matter.set_tier(obj, "soft", { stiffness = 0.4, volume = 0.9 })
matter.set_sticky(obj, { strength = 0.7, break_strain = 2.5 })
matter.blend_into(obj, fractal, { k = 0.0 → 0.8 over 2.0 })   -- ramp k, "soup"
matter.wake(obj)
```

Everything is **RON**-serialized like the rest of Floptle, so matter setup is
diffable and hand/AI-editable.

```ron
// a soft, sticky blob that melts into the fractal
MatterModel(
    tier: SoftBody(XpbdCage(
        particles: 512,
        stiffness: 0.35,          // → compliance α = 1/stiffness
        volume_preserve: 0.9,
        shape_match: 0.5,
    )),
    blend: "goo",                 // material id; rules live in the matrix below
    sleep: Awake,
)

// material-interaction matrix snippet (RON)
InteractionMatrix(rules: {
    ("goo", "goo"):        Merge(k: 0.6),    // goo fuses with goo — soup
    ("goo", "fractal"):    Merge(k: 0.8),    // melt the blob into the fractal
    ("goo", "metal"):      Carve(k: 0.1),    // goo bites a dent into metal
    ("metal", "metal"):    Hard,             // crisp boolean, no blending
    ("goo", "glass"):      Ignore,           // pass through, no interaction
})
```

## 7. Near-term vs future (the honest roadmap)

**Near-term — achievable now:**

- Tier 1 **Morph** (GPU displacement + baked-field collision).
- Tier 2 **FieldBlend** (smin/smax + the interaction matrix) — the "soup," carve,
  reject, and "blend into a fractal" behaviors.
- Tier 3 **basic XPBD soft body** (distance + volume + shape-match, SDF collision,
  skinned mesh).
- Tier 4 **simple adhesion** (sticky bonds, stretch, single-bond snap).

**Future / research-grade — flagged, not promised:**

- Full **MPM** (MLS-MPM/APIC) for true unified goo/sand/snow soup and high-quality
  viscoelasticity.
- Robust **topological fracture into strands** with elastoplasticity + damage and
  stable strand remeshing.
- Real-time **mesh splitting** into independent objects (severing the constraint
  graph into components live).
- High-quality **anisotropic surface reconstruction** of large particle soups back
  to a crisp renderable field.

## 8. Out of scope / honesty

This is hard, and we ship the ladder one rung at a time. We are **not** promising
film-VFX-grade destruction, fluid sims, or fracture at launch — the early tiers
deliver the distinctive look (morph, soupy blends, jiggly soft bodies, simple
stickiness) while MPM and true fracture stay explicit later milestones. Critically,
**nothing in the renderer or the SDF collision world has to change to add a tier**:
every tier is just another writer into the shared field behind the same
`CollisionShape`/raymarch contract — so the risk is concentrated in
`floptle-matter`'s solvers, not spread across the engine.
