# Floptle — Time (`floptle-core::time` + `floptle-rules`)

Promote time from a single global scalar to a **rate field `r(p)`**, so a region can
slow, freeze, or dilate the flow of time the same way a region already bends gravity
or light — locally, spatially, deterministically.

> Decision & rationale: [`../decisions/0017-time-as-a-field.md`](../decisions/0017-time-as-a-field.md).
> Reads-with: the SDF sim [`./physics.md`](./physics.md), the matter that morphs over
> time [`./deformable-matter.md`](./deformable-matter.md), the reference-frame tree
> [`./large-world-space.md`](./large-world-space.md), and the law-axis authoring model
> [`./world-rules.md`](./world-rules.md) (the `floptle-rules` crate, ADR-0018).

The clock and the fixed-step loop already live in `floptle-core`. This document adds
one struct (`LocalTime`) next to them and one component (`TimeRegion`) that feeds a
sampler, then shows how the four time-consuming systems — morph, particles, the
physics integrator, animation — read an entity-local `dτ` instead of the global `dt`.
Nothing else changes. That is the whole point: time was *almost already* a field.

## 1. The gap

Floptle's thesis is that a game is a simulated universe whose developer-defined rules
**compose** because everything is a field sampled at `(p, t)` over one SDF substrate.
We have been good about the `p`: space, gravity, density, and light are all promoted
to fields you can author and layer. We have been lazy about the `t`. There is exactly
one global scalar `t`, incremented once per fixed step, and it is threaded — identical
— into every `f(p, t)` in the engine.

That asymmetry is the bug. And the fix is closer than it looks, because the engine's
signature trick is *already a time derivative*:

> Deformable matter drives surface velocity from `∂f/∂t` (ADR-0012): the morph carries
> things along because the field is changing *in time*. The carry magic is `d/dt`.

If the rate of `t` were a function of position, `∂f/∂t` would scale with it for free —
a frozen region would stop carrying, a fast region would carry harder — and we would
not have to teach any downstream system a new concept. So: promote `t`.

## 2. Core mechanism — one rate field, many local clocks

A scalar **rate field** `r(p)` says "how fast does proper time pass here," with `1.0`
meaning real-time. Each warped entity carries a tiny **local clock**:

```rust
// floptle-core::time  (next to the existing master clock + fixed-step loop)

/// Per-entity proper-time accumulator. Default = global time (rate 1.0).
struct LocalTime {
    tau:         f64, // entity-local proper time, monotonic
    accumulator: f64, // unspent local time, drained in fixed sub-steps
}

impl LocalTime {
    /// Called ONCE per body per global step, at the quiet point.
    /// `rate` was sampled at the start-of-step position; `dt` is the fixed step.
    /// Returns how many local sub-steps this body runs this global step.
    fn advance(&mut self, rate: f64, dt: f64, max_sub: u32) -> u32 {
        self.accumulator += rate * dt;          // dτ = r(p)·dt
        let mut n = (self.accumulator / dt) as u32;
        if n > max_sub { n = max_sub; }          // HARD CAP — degrade rate, not framerate
        self.accumulator -= (n as f64) * dt;     // keep the remainder for next step
        self.tau += (n as f64) * dt;
        n
    }
}
```

`dτ = r(p)·dt`. Everything surreal falls out of this one line:

```
 r = 0.0   freeze        r = 0.25  heavy slow / bullet-time
 r = 1.0   real-time     r = 4.0   fast-forward
 r ∈ (0,1) dilation      r > 1     time-acceleration zone
```

And it composes for **free** with morphing worlds, because a morph *is* a function of
`t`. Set `r = 0` over a swirling fractal and it freezes mid-swirl while the identical
fractal one meter away keeps churning — no special-casing in the morph system, just a
different `dτ` going in. This is the literal **temporal twin** of the hierarchical
reference-frame tree (ADR-0015): that tree localizes *space* per frame; `LocalTime`
localizes *time* per body. Same shape, different axis.

## 3. Determinism — non-negotiable

The master clock stays in charge. We are not building per-object wall-clock timers;
those drift and would wreck both replay and the future networking goal
([`./networking-future.md`](./networking-future.md)). The **global fixed step is the
authoritative master clock**; warped entities only ever *drain* whole sub-steps from
their accumulator. Five invariants make the whole thing replayable bit-for-bit:

1. **Sample `r` once per body per global step**, at the entity's *start-of-step*
   position. No mid-sub-step re-sampling — that would couple a body's motion to its own
   rate within a step and make ordering observable.
2. **Fixed iteration order.** Bodies advance in a stable, content-independent order
   (e.g. by `EntityId`), so float accumulation is reproducible.
3. **Forbid self-reference cycles.** `r(p)` may not read a quantity it is currently
   advancing (no "this region's rate depends on how fast things inside it move").
   Validated at lawset-build time; a cycle is a hard error, not a runtime surprise.
4. **Hard-cap sub-steps per global step** (`max_sub`). A fast zone (`r = 20`) does not
   get to consume 20× the frame budget; it runs `max_sub` sub-steps and *banks* the
   rest in the accumulator. The zone degrades in **rate**, never in framerate, and
   never in determinism.
5. **Advance at the quiet point.** `LocalTime::advance` runs at the same between-step
   barrier as the floating-origin rebase — after the global step count increments,
   before any system reads `dτ`. Advance is a pure function of the global step count.

```
global step N
   ├─ rebase floating origin        (ADR-0015, the "quiet point")
   ├─ for body in stable_order:
   │      r = sample_rate(body.start_pos)      // ONCE
   │      n = body.local.advance(r, DT, MAX)   // pure, capped
   │      body.sub_steps = n                   // 0..=MAX
   └─ run systems, each consuming body.sub_steps · DT  (never global dt)
```

## 4. Authoring — a region, or a law axis

The near-term authoring surface is a single component: a box or sphere with a rate
knob. Inside its bounds, that rate wins (composited with the ambient `r`).

```rust
struct TimeRegion {
    shape: RegionShape,   // Box(Aabb) | Sphere(center, radius) — reuses field bounds
    rate:  f64,           // r inside the region; 0.0 = freeze, 1.0 = normal
    falloff: Falloff,     // Hard | Smooth(width) — soft edge so it desaturates, not snaps
    priority: i16,        // resolves overlap; higher wins (then min-rate as tiebreak)
}
```

```ron
// world.ron — a bullet-time dome around an arena
TimeRegion(
    shape:   Sphere(center: (0.0, 2.0, 0.0), radius: 12.0),
    rate:    0.2,                  // everything inside runs at 1/5 speed
    falloff: Smooth(width: 1.5),   // 1.5 m feathered shell
    priority: 10,
)
```

The same value is also expressible as the **time-rate law axis** of a `Realm`/`Lawset`
(see [`./world-rules.md`](./world-rules.md)): a realm that bends gravity *and* slows
time is just two axes set on one lawset, and they compose through the same field
interaction graph (ADR-0019) as everything else. `TimeRegion` is the convenient,
spatially-bounded special case; the law axis is the general one.

```lua
-- Lua API: set a region's rate, or scale one body's clock directly.
floptle.time.set_region_rate(arena_dome, 0.2)   -- whole region
floptle.time.set_body_scale(boss_entity, 0.5)   -- one entity, local override
local tau = floptle.time.tau(player)             -- read proper time
```

## 5. Data flow — who consumes `τ`

Every time-stepped system already takes a `dt`. The only change is *which* `dt`: each
reads the entity's `sub_steps · DT` (its local `dτ` for this global step) instead of
the global `dt`.

```
sample r(p) ─┐
             ├─► LocalTime.advance ─► sub_steps (0..=MAX) ─┬─► morph:    f(p, τ)        ∂f/∂τ → surface vel
master DT ───┘                                            ├─► particles: age/emit by dτ
                                                          ├─► integrator: XPBD steps × dτ
                                                          └─► animation: clip cursor += dτ
```

- **Morph** (deformable-matter): evaluates `f(p, τ)` and its `∂f/∂τ`. Freeze (`r=0`)
  yields zero sub-steps → the field is static *and* its surface velocity is zero, so
  carried bodies stop too. No new branch.
- **Particles** (particles-vfx): age, emission cadence, and forces all integrate over
  `dτ`. A particle that crosses a dome boundary slows because its body's `r` changed —
  emergent, not scripted.
- **Physics integrator**: runs `sub_steps` XPBD/position iterations of `DT` each. The
  hard cap is what keeps a fast zone from exploding the solver budget.
- **Animation**: the clip cursor advances by `dτ`, so a slowed actor's walk cycle
  slows in lockstep with its physics — they cannot desync, because they share `τ`.

## 6. Payoff — surreal + gameplay

- **Bullet-time domes**: step into the shell, the world drops to `r=0.2`, you don't.
- **Frozen/slowed zones**: a stasis field that holds a collapsing bridge mid-fall; a
  swamp where everything (you included) wades through slow time.
- **Dilation fields**: a gradient `r(p)` near a singularity — clocks crawl as you
  approach, exactly as the gravity field steepens beside it.
- **Puzzle/combat/exploration**: freeze a hazard to cross it; slow a projectile to
  weave through; accelerate a plant-growth region to build a bridge.
- **Post pairs with it for free**: the post stack reads `r` along the view ray and
  desaturates / adds motion-trail inside a slowed region, so time-warp *reads* on
  screen without any per-effect bookkeeping. The `Smooth` falloff is why the grade
  fades in rather than snapping at the boundary.

## 7. Research / deferred — direction and echoes

Honest tiering, same as the gravity Poisson tier: **scale ships first; direction and
rewind are a later, large item.**

- **Time-direction (`r < 0`, reverse)** and **echoes/rewind** require *replaying state
  backward*, which is genuinely hard over stateful, irreversible simulation — plastic
  deformation and fracture in XPBD/visco matter (ADR-0013) are not invertible by
  negating `dτ`.
- The plan, when we get there, is **bounded ring-buffer traces of *tagged* entities
  only** — never the whole world. A tagged body records a fixed-length history of its
  reconstructable state; rewind replays *that buffer*, it does not re-derive physics
  backward. Cost and correctness stay bounded because the trace set is opt-in and
  small.
- It is deliberately the *same code path*: a negative or buffered `dτ` flows through
  the identical `advance` → `sub_steps` → systems pipeline. We ship the seam now and
  grow echoes into it later, rather than retrofitting.

## 8. Out of scope (at launch)

- **Full rewind of arbitrary world state.** Only tagged-entity traces, and only later.
- **Relativistic time.** No Lorentz factors, no light-cone causality. `r(p)` is an
  authored gameplay knob, not physics.
- **A global slow-mo button.** Trivial (scale the master `dt`) and *not the point* —
  the point is **local, spatial** time rules that compose with the rest of the field
  stack. We will not ship the global button as if it were the feature.

## 9. Near-term proof — the thin seam

The engine is ~100:1 docs-to-code; this lands as a **thin seam shipping the smallest
honest proof first**, never a big-bang. One scene, one component, one barrier hook:

> A single `TimeRegion` dome at `r=0.2` over a corner of the world. Inside it: the
> morphing fractal slows mid-swirl, the particle fountain drips instead of sprays, and
> a pendulum swings in slow motion — **while the rest of the world runs at `r=1`.**
> Walk the player in and out across the `Smooth` shell and watch the boundary, not a
> hard line.

That proof exercises the entire load-bearing path: `sample_rate` → `LocalTime::advance`
(with the hard cap and stable order) → `sub_steps` feeding morph + particles +
integrator + animation, all at the quiet point next to the floating-origin rebase. It
is the exact code path that later grows time-direction and echoes — so the proof is not
a throwaway demo, it is the foundation, validated small before it carries weight.
