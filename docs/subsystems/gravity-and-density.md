# Floptle — Gravity & Density (`floptle-physics::gravity`)

Gravity is not a global constant — it is a composable **vector field** `g(p)` you sample as "down," and density is a first-class material property from which mass, the gravity matter emits, and crush/compaction all follow.

> Decision & rationale: [`../decisions/0014-gravity-fields.md`](../decisions/0014-gravity-fields.md), with density-as-matter from [`../decisions/0013-deformable-matter.md`](../decisions/0013-deformable-matter.md).
> Reads-with: the SDF collision core [`./physics.md`](./physics.md) (gradient, character controller, baked brickmap) and the matter solver [`./deformable-matter.md`](./deformable-matter.md) (compaction, XPBD).

`gravity` is a module inside **`floptle-physics`**. It consumes scalar/vector
**spatial fields** from **`floptle-field`** (which now holds density `ρ(p)`,
potential `Φ(p)`, and acceleration `g(p)` alongside SDFs and CSG) and is read by the
**character/vehicle controllers** in this same crate. `floptle-matter` *writes*
per-material density into the field; gravity *reads* it. One-way flow, no cycle.

## 1. Core principle — space, matter, gravity, density are one thing

The engine already understands **space** (the SDF `f(p, t)`) and **matter**
(`MatterModel` over that field). Gravity closes the loop with two equations:

```
mass     m  = ρ · V          (V = volume from the SDF/mesh; ρ = material density)
gravity  g(p) = field emitted by that mass, sampled anywhere in space
```

So "the more mass/density, the stronger the gravity that matter creates" is the
literal data path: density is authored per **material**, volume comes from the shape
the engine already has, mass falls out, and from mass comes the field everything
(you, a car, a spaceship, debris) samples as `g(p)`. There is **no** global
`(0,-9.8,0)` baked anywhere — that is merely the cheapest tier, entirely opt-in.

> The payoff the vision asks for: if a fractal swirls up into the air, the *space
> itself emits a pull* that keeps you grounded, so you **run up that wall** — not
> anti-gravity floatiness, but a real "down" that follows the surface.

## 2. The composable gravity tiers (cheapest → heaviest)

Same philosophy as the rest of Floptle: pick the cheapest tier that gets the feel;
a tier never drags in the machinery of the tiers above it.

```
cost  ▁▁▂▂▃▃▅▅▇▇█
      Global   Sources   SDF-surface   Density-field
       │         │           │              └ Poisson ∇²Φ=4πGρ, FFT/multigrid/Barnes–Hut (research)
       │         │           └ g ∝ -∇f, reuses physics' gradient — nearly free (the headline)
       │         └ Newtonian point/sphere/line, O(bodies×sources)
       └ constant vector or authored regions (most games)
```

### Tier 0 — Global / volume gravity (most games)

A constant vector, or authored **gravity regions** (a box or sphere with either a
fixed direction or a radial pull toward/away from a center):

```
constant:   g(p) = g0                                   // e.g. (0, -9.8, 0)
region box: g(p) = dir · strength      if p ∈ box       // local "down" volume
region rad: g(p) = -strength · normalize(p - c)         if |p-c| < R  (radial well)
```

Evaluated by a point-in-volume test; effectively free. The default for any game that
just wants things to fall down. Regions let a gravity-flip room, an elevator shaft,
or a stylized vortex exist without any physics tier.

> *Use when:* normal games, authored set-pieces. Cost: a few AABB/sphere tests.

### Tier 1 — Analytic sources (planets, O(bodies × sources))

Point / sphere / line sources with **Newtonian** falloff — the planet tier:

```
point/sphere:  g(p) = -G · M · (p - c) / |p - c|³          (M = ρ·V of the body)
line/rod:      g(p) = -2G·λ · (p - c⊥) / |p - c⊥|²         (λ = mass per length)
```

`G` is a **game-feel** constant, not 6.674e-11 — tune it for your scale (see §10). A
cheaper, jitter-free alternative for "stand on a planet" ignores `1/r²` and just
points at the center with constant strength inside an outer radius:

```
simple planet:  g(p) = -strength · normalize(p - c)        for |p-c| < R_outer
```

`O(bodies × sources)` — trivial for a handful of planets. The Mario-Galaxy /
Outer-Wilds tier: walk **around** a planet (down points at its center), orbit it,
fly between several. Multiple sources just sum (§3).

> *Use when:* space exploration with planets/moons; orbital flight; "walk all the way
> around a small planet." Cost: a few vector ops per source per body.

### Tier 2 — SDF-surface gravity (the headline)

Pull a body **onto the nearest implicit surface** so "down" follows the geometry —
this is what makes you **run on a fractal and up its swirling walls**:

```
g_sdfSurface(p) = -strength · normalize(∇f(p)) · falloff(f(p))
```

`∇f(p)` is the SDF gradient — the **exact same gradient** physics computes for
contact normals and the renderer for shading ([`./physics.md`](./physics.md) §"The
SDF math": analytic where available, else the 4-tap tetrahedron difference). So this
tier is **nearly free**: it reuses an evaluation the frame already needed.
`-normalize(∇f)` points from `p` *into* the nearest wall/floor — your local "down,"
whatever wild angle the fractal folds into.

```
   fractal wall swirls up        g points INTO the surface (toward it),
   ───────╮                      so the player's "down" tracks the wall:
          │  ● player  → g       ● runs straight up the vertical face,
          │  ↘ down              kept grounded by the field, not floaty
   ───────╯
```

**Smoothing (avoid jitter).** Near a deep fractal `∇f` flips wildly between nearby
surfaces. Three fixes, in order of preference:

- Sample the gradient of the **baked / blurred distance field** (the brickmap,
  [`./physics.md`](./physics.md) §"Baked sparse SDF"), not the raw analytic fold —
  trilinear interpolation already low-pass-filters it, so its gradient is smooth.
- **Widen `ε`** in the finite difference toward the field's Lipschitz scale, trading
  feature sharpness for stability (the `ε` tradeoff `physics.md` calls out).
- **Temporally smooth** the resulting `g` per body (EMA) so the up-vector eases.

**Range clamping.** `falloff(d)` zeroes the pull far from any surface so you are not
yanked toward distant geometry: e.g. `falloff(d) = clamp(1 - d/R_grav, 0, 1)`, full
on the surface, gone past `R_grav`. Beyond range this tier contributes nothing and
you blend back to whatever else is active (§3) — leap off the fractal and you fall
under Global/Source gravity until you near a surface again.

> *Use when:* infinite fractal block-worlds; walking arbitrary morphing implicit
> geometry; "run up the wall." Cost: ~one gradient eval — usually already cached for
> collision that step.

### Tier 3 — Calculated density-field gravity (research / later)

The literal "engine calculates gravity from mass and density" tier. Derive a
**density field** `ρ(p)` from matter — per-material density times SDF occupancy on
the sparse brickmap — then solve for the potential and take its gradient:

```
density:    ρ(p) = Σ_materials  ρ_mat · occupancy_mat(p)     (occupancy from f<0 on the brickmap)
Poisson:    ∇²Φ(p) = 4πG · ρ(p)                              (gravitational Poisson eq.)
gravity:    g(p) = -∇Φ(p)
```

The only tier where gravity is *emergent*: a dense fractal core genuinely out-pulls
a hollow shell, and a compacted clay heap (§5) gains pull as its density rises. The
trap is cost — naïve O(N²) n-body or a dense Poisson solve is a non-starter. We do
**not** do that. Pick by structure:

- **Continuous field (the brickmap is a grid):** solve `∇²Φ = 4πGρ` with a **grid
  Poisson solver** — an FFT (particle-mesh) solve on a padded block, or **geometric
  multigrid** for non-periodic boundaries. Both are `O(N)`–`O(N log N)` in cells, run
  as wgpu compute, and reuse the sparse bricks (empty space contributes nothing).
- **Many discrete masses (debris, ships, chunks):** a **Barnes–Hut octree** treecode
  (group distant mass into one multipole), `O(N log N)`, or **FMM** for `O(N)` at
  large `N`. The standard computational-astrophysics answer; we don't invent it.

**Low cadence + LOD.** Gravity changes *slowly*, so the solve runs at a **low
cadence** (every Nth step / a rolling budget) and `g(p)` is sampled cheaply
(trilinear on `Φ`) every step between. Far/coarse bricks solve lower-res and slower.
Bodies read the cached field; only the *re-solve* costs, and it is amortized.

> **Honesty.** For bounded scenes this is achievable. For **huge/infinite worlds**,
> a globally consistent density-field solve is **later/research** — the near-term
> engine ships Tiers 0–2 (which already deliver fractal-walking and planets) and
> treats Tier 3 as the long arc. Flagged, not promised.

## 3. Composition — sum of opt-in fields

A body's total gravity is the **sum** of whatever tiers are enabled for it:

```
g(p) = g_global + Σ_s g_source(p, s) + g_sdfSurface(p) + g_densityField(p)
```

Each term is opt-in and independently weighted; absent tiers cost nothing. The
surface range clamp (§2.2) and source falloff keep the blend smooth — leap off a
fractal wall and `g_sdfSurface` fades while `g_global` takes over, so you arc into a
clean fall rather than snap.

```rust
/// What a body asks the world for, each fixed step, at its position.
pub struct GravityField {
    pub global:  Option<GlobalGravity>,   // constant or region lookup
    pub sources: Vec<GravitySource>,      // analytic planets/rods (Tier 1)
    pub sdf:     Option<SdfGravity>,      // surface-follow (Tier 2): strength, R_grav, smoothing
    pub density: Option<DensityGravity>,  // sampled Φ grid handle (Tier 3)
}

pub enum GravitySource {
    Point  { c: Vec3, mass: f32, g_const: f32 },           // Newtonian or simple-radial
    Sphere { c: Vec3, mass: f32, radius: f32, r_outer: f32 },
    Line   { a: Vec3, b: Vec3, lambda: f32, r_outer: f32 },
}

impl GravityField {
    /// The workhorse: total "down" at p. Reuses physics' ∇f for the sdf term.
    pub fn sample(&self, p: Vec3, world: &CollisionWorld, t: f64) -> Vec3 {
        let mut g = Vec3::ZERO;
        if let Some(gl) = &self.global  { g += gl.at(p); }
        for s in &self.sources          { g += s.at(p); }
        if let Some(sd) = &self.sdf     { g += sd.at(p, world, t); }   // -strength·n̂·falloff
        if let Some(df) = &self.density { g += df.sample(p); }         // -∇Φ, trilinear
        g
    }
}
```

A `Gravity` component (§7) carries the per-body `GravityField` (or a shared
reference). The controller calls `sample` once per step and treats the result as the
only "down" it knows.

## 4. Movement in a gravity field

The kinematic character controller ([`./physics.md`](./physics.md) §"Kinematic
character controller") needs only one change: **"up" is `-normalize(g(p))`**, not a
hardcoded `+Y`. Everything else — move-and-slide on `spherecast`/`sweep`, ground
probe, step offset, `v_surf` carry — runs in that **local frame**:

```
1. g   = gravity.sample(pos, world, t)
2. up  = -normalize(g)                      // field-defined "up"
3. align body orientation toward `up` SMOOTHLY (slerp, not snap):
      target = look_rotation(forward_projected_onto_plane(up), up)
      orient = slerp(orient, target, align_rate · dt)
4. accelerate along g (when airborne), then move-and-slide in the {forward, up} frame
5. ground check = short spherecast along -up; grounded if hit & angle(n, up) ≤ slope_limit
6. inherit v_surf from the ground contact (∂f/∂t) as before
```

Because the controller is expressed in the local `up`, the **same code** yields
wall-running (Tier 2 up tracks a vertical fractal face), planet-walking (Tier 1 up
points at the planet center as you circle it), and arbitrary fractal-surface-walking
— no special cases, just a different `g`.

```
  flat ground         around a planet          up a fractal wall
   up↑                    up                       up→
   ●                   ╱  ●  ╲                  ┌───●  (up points out of
  ─────              (   ●●●   )  ← walk        │   ↑   the wall; player
                      ╲   ●  ╱     all the way  │       walks the vertical
                         up        around       └───    face, grounded)
```

**Smooth alignment matters.** Snapping orientation to a fast-changing fractal normal
is nausea-inducing; the `slerp` in step 3 (plus the temporal smoothing from §2.2)
gives stable, readable reorientation. The **camera** follows the same `up`: the
controller exposes the current up-vector and the camera rig
([`./camera-and-dialogue.md`](./camera-and-dialogue.md)) eases its own up toward it,
so the horizon rolls with the surface instead of the world spinning under a fixed
camera.

**The spaceship case.** A free-flying body sets its **gravity response** low or zero:
it samples `g(p)` (for HUD "down," or a gentle pull when near a planet) but
integrates under its own thrust. A `response: f32` scales how much of `g` it obeys:

```
v += (response · g(p)) · dt        // response = 1 grounded walker, ≈0 free spaceship
```

So one planet can be **orbited in a ship** (low response, Newtonian source pull)
**and landed on and walked** (response = 1, source gravity + optional surface
alignment) — same field, different response.

## 5. Density → matter behavior (crush & compaction)

Density is not only mass and gravity — per ADR-0013 it governs whether matter
**crushes** under pressure (soft clay) or **resists** (hard metal). This ties into
the XPBD soft-body / matter solver ([`./deformable-matter.md`](./deformable-matter.md)
§"Soft body"). Two material constants drive it:

```
mass:           m = ρ · V
bulk modulus K: resistance to volume change      → volume-constraint stiffness
yield strength: pressure past which compaction is PLASTIC (permanent)
```

**Pressure → strain.** A material under pressure `P` changes volume by the linear
elastic relation, and density rises as volume falls (mass is conserved):

```
volumetric strain:   ΔV/V = -P / K                 (large K ⇒ tiny strain ⇒ "hard")
new density:         ρ' = ρ · V / V' ≈ ρ · (1 + P/K)
```

**Elastic vs plastic.** Below the yield pressure the strain is **elastic** — it
springs back when load is removed. Above it, compaction is **plastic** — the rest
volume `V₀` itself permanently shrinks (and `ρ` permanently rises):

```
if P ≤ P_yield:   elastic   — V relaxes back to V₀ when unloaded
if P >  P_yield:  plastic   — V₀ ← V₀ · (1 - (P - P_yield)/K)   (compaction sticks)
```

In XPBD terms this is exactly the **volume constraint** `C = V(tet) − V₀` with
**compliance `α = 1/K`** (high `K` ⇒ stiff, near-incompressible metal; low `K` ⇒
soft clay) plus a **plastic yield** on volume strain that rewrites `V₀` — the same
elastoplastic return-mapping the matter doc uses for stretch-to-strands, applied to
*volume* instead of *length*.

**Where pressure comes from.** Contacts (something pressing on the matter) **plus
gravity load** — a tall dense column self-loads under its own `g`, so the bottom
bricks feel `P ≈ ρ · |g| · h` and compact first. Closing the loop: compaction
raises `ρ`, which (Tier 3) raises the gravity that region emits.

```
  load ↓↓↓                soft clay (low K):           hard metal (high K):
  ┌─────────┐  P>P_yield   ┌─────────┐  squashed &      ┌─────────┐  barely
  │ ▓▓▓▓▓▓▓ │  ──────────▶ │▓▓▓▓▓▓▓▓▓│  permanently     │ ▓▓▓▓▓▓▓ │  moves
  │ ▓▓▓▓▓▓▓ │              └─────────┘  denser (ρ↑)      └─────────┘  (springs back)
  └─────────┘
```

> Near-term ships density→mass and **basic** elastic+plastic volume compaction on
> Tier-3 soft bodies; rich, large-scale plastic flow is the matter doc's research arc.

## 6. Performance posture

Hyperoptimization is the requirement (ARCHITECTURE §9); the tier ladder *is* the
optimization:

- **Tiers + opt-in.** Most games never leave Tier 0 (a few AABB tests); sources are a
  handful of vector ops. You pay only for the tier you reach for.
- **SDF-surface gravity is nearly free** — reuses the `∇f` physics already evaluates
  for collision that step ([`./physics.md`](./physics.md)), usually a cache hit.
- **Density-field Poisson at low cadence + LOD.** Solved on the **sparse brickmap**
  (empty space free), re-solved every Nth step on a rolling budget, coarser/slower
  with distance; `g` is sampled trilinearly from cached `Φ` cheaply each step.
- **Barnes–Hut / FMM** for discrete masses — `O(N log N)`/`O(N)`, never O(N²).
- **GPU compute.** The Poisson solve (FFT/multigrid), treecode, and per-body `g`
  sampling run as wgpu dispatches; the CPU orchestrates.
- **Fixed-step determinism.** Sampling and solve run in the fixed-step stage;
  deterministic ordering keeps the sim reproducible — a head start on the **future
  networking** goal ([`../ARCHITECTURE.md`](../ARCHITECTURE.md) §10).
- **Sleep/wake.** Grounded idle bodies sample lazily; sleeping matter
  ([`./deformable-matter.md`](./deformable-matter.md) §5) does not re-solve its
  density contribution until disturbed.

## 7. Authoring & scripting

**Gravity component / inspector.** A `Gravity` component picks which tiers a body
participates in and how strongly:

```rust
struct Gravity {
    field:    GravityField,   // global? sources? sdf? density? (§3)
    response: f32,            // how much this body obeys g (1 = walker, ~0 = spaceship)
    align:    AlignMode,      // None | UpToField(rate)   — orientation follow (§4)
}
```

The inspector exposes tier toggles, per-tier strength/range (`R_grav`, source mass or
`g_const`, smoothing), `response`, and align rate. A body *emits* gravity by carrying
a `GravitySource` (mass = material density × SDF volume) — "this rock is heavy enough
to pull things" is a checkbox, not code.

**Per-material density.** Density lives on the **material** (alongside the matter
constants of [`./deformable-matter.md`](./deformable-matter.md) /
[`./materials-and-textures.md`](./materials-and-textures.md)): `density`,
`bulk_modulus`, `yield_strength` — authoring a material authors its mass-per-volume,
crushability, and (via Tier 3) emitted gravity all at once.

**Lua API** ([`../ARCHITECTURE.md`](../ARCHITECTURE.md) §6) — the runtime control
surface:

```lua
gravity.set_mode(obj, "sdf_surface", { strength = 18.0, range = 4.0 })  -- pick a tier
gravity.add_source(planet, { mass = 5.0e6, g_const = 12.0 })            -- emit gravity
gravity.set_response(ship, 0.0)                                         -- free flight
gravity.align_to_field(player, true)                                   -- wall-running on
local g = gravity.sample(some_point)                                   -- query g at a point
```

Everything is **RON**-serialized like the rest of Floptle — diffable, hand/AI-editable:

```ron
// A fractal world you run on, with one orbital planet and two materials.
Gravity(
    field: GravityField(
        global:  None,                          // no global "down" — the surface defines it
        sdf:     Some(SdfGravity(               // Tier 2: run on / up the fractal
            strength: 18.0,
            range:    4.0,                      // R_grav: pull only near a surface
            smoothing: Baked(blur: 0.5),        // gradient from the blurred brickmap
        )),
        sources: [                              // Tier 1: a planet you can also orbit
            Sphere(c: (0, 0, 900), mass: 5.0e6, radius: 200, r_outer: 1200),
        ],
        density: None,                          // Tier 3 off (near-term)
    ),
    response: 1.0,                              // grounded walker obeys g fully
    align:    UpToField(rate: 8.0),             // smooth reorientation to the surface
)

// Per-material density / compaction (read by gravity AND the matter solver).
Material(id: "soft_clay", density: 1.6, bulk_modulus: 0.05, yield_strength: 0.2)  // crushes
Material(id: "hard_iron", density: 7.8, bulk_modulus: 8.0,  yield_strength: 6.0)  // resists
```

## 8. Use cases mapped to tiers

- **Infinite fractal block-world — run on the surfaces.** Tier 2 (SDF-surface
  gravity) + Tier 0 fallback when airborne. "Down" follows the fold; you run up the
  swirling wall, kept grounded. No anti-gravity floatiness.
- **Space exploration with fractal planets.** Tier 1 (analytic sources) for the
  planets' pull; the **spaceship** flies free (`response ≈ 0`), feeling each planet
  only as it nears. Procedural fractal planets are just SDF bodies with a
  `GravitySource` whose mass comes from `ρ·V`.
- **A planet you orbit AND land on.** One `Sphere` source: orbit it in the ship
  (low response, Newtonian falloff for the orbit), then land and walk all the way
  around (response = 1, source gravity points at the center; optionally add Tier 2
  surface alignment for fractal terrain on the planet).
- **Emergent gravity from a dense core (later).** Tier 3 — a procedurally dense
  fractal genuinely out-pulls a hollow one; compacted clay (§5) gains pull as `ρ`
  rises. Research arc.

## 9. Near-term vs future (the honest roadmap)

**Near-term — achievable now:**

- Tier 0 **Global / volume** gravity (constant + authored regions).
- Tier 1 **Analytic sources** (point/sphere/line, Newtonian or simple-radial).
- Tier 2 **SDF-surface gravity** — the headline; reuses `∇f`, smoothing, range
  clamp, blend with other tiers. Wall-running / fractal-walking ships here.
- **Density → mass** (`m = ρ·V`) and **basic compaction** (elastic volume strain +
  simple plastic yield on Tier-3 soft bodies).
- Character controller **field-up + smooth alignment**; spaceship `response`.

**Future / research-grade — flagged, not promised:**

- Tier 3 **calculated density-field Poisson gravity** (FFT/multigrid + Barnes–Hut/
  FMM) at scale, especially for **huge/infinite** worlds with a globally consistent
  field.
- **Rich plastic compaction** (large-scale plastic flow, density-driven phase-like
  behavior) beyond the basic volume-yield.
- Two-way **gravity↔compaction feedback** (compaction raises `ρ`, raises emitted
  gravity) at world scale.

## 10. Out of scope / honesty

We want **plausible, controllable, game-feel** gravity — not a science simulator.

- **No general relativity**, no relativistic corrections, no realistic astrophysical
  accuracy. `G` is a tuning knob, not a physical constant.
- **No realistic orbital mechanics** as a goal — orbits are stable and fun, not
  ephemeris-accurate.
- Gravity is **opt-in per game**: the engine makes it *easy* when desired (because it
  inherently understands space, matter, gravity, and density), and *absent* when
  not. Most games stay at Tier 0 and never think about any of this.

Nothing here changes the renderer or the SDF collision world: gravity is a *reader*
of the shared field and a per-step "down" for controllers that already move in a
local frame — so the risk is concentrated in `floptle-physics::gravity` and the
Tier-3 solver, not spread across the engine.
