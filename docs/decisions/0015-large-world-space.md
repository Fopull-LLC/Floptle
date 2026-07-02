# ADR-0015 — Large-world space: floating origin, on by default

- **Status:** Accepted · 2026-06-27 · layers 1–3 implemented 2026-07-02
- **Decider:** Ty Johnston (Fopull LLC)

> **Implementation note (2026-07-02).** Layer 2 landed as an **origin-relative
> physics frame**, not a world mutation: `PhysicsWorld.origin` (f64) anchors the
> sim; bodies/contacts/ray origins are origin-relative, each static collider is
> baked relative to its own f64 anchor, and a rebase (camera > 4 km from the
> origin) recenters the frame by a whole-number shift and recomputes collider
> offsets *from the f64 anchors* — zero accumulated error, and **user-visible
> coordinates never change** (scripts/ECS keep stable f64 world positions, so a
> Lua variable holding a position survives any rebase). Writeback to the ECS is
> also **interpolated** by the fixed-step accumulator fraction, which fixed the
> long-standing "player jerks back and forth while moving" temporal-aliasing bug.
> **Terrain is covered too:** per-volume fields are node-local by construction,
> and the combined field is GONE — the renderer holds every volume in one 3D
> atlas at native resolution and fuses them in-shader (smin, air-continued
> outside boxes, union-edge taper), each placed by composing its node's `f64`
> anchor + local center before going camera-relative; physics gives each volume
> its own anchored collider at native resolution. Neither rendering nor
> collision degrades with distance from the origin or between volumes.
> Proof: `terrain_far_probe` renders the two-volume `terrain_blend_probe` scene
> ten million units out — the PNGs are bit-identical.

## Context
`f32` precision degrades as coordinates grow: far from the origin the gap between
representable values exceeds a meter, causing **jitter** in rendering, physics,
and transforms. GPUs are `f32`-native, so even an `f64` world must be reduced to
`f32` for the GPU. This is a near-universal problem most engines make the
developer solve by hand. The vision includes **galaxy-scale** worlds (procedural
fractal planets, infinite fractal depth) — so the engine must **just handle it,
behind the scenes, with zero developer work**: the world moves around the player,
the player stays near the origin.

## Decision
Make the engine's default coordinate space **large-world-safe and origin-relative**,
layered (all transparent; developer APIs use ordinary world space):

1. **Camera-relative rendering** *(always on)* — positions are uploaded to the GPU
   **relative to the camera** (model-view formed in high precision, cast to `f32`
   last). No GPU jitter at any world scale. Cheap; the default render path.
2. **Floating origin** *(always on)* — the active simulation is kept near
   `(0,0,0)`; when the camera passes a threshold, the world is **rebased** by the
   offset (positions shift; velocities/forces are translation-invariant, so they
   don't) between fixed steps, so physics never sees large coordinates.
3. **`f64` authoritative transforms** — world positions are double precision,
   covering planet / solar-system scale at sub-millimeter precision.
4. **Hierarchical reference frames** — for galaxy-and-beyond (a galaxy ≫ `f64`
   meter-precision), nested frames (galaxy → system → body → local) carry
   high-precision parent offsets; only the player's **local** frame is simulated
   at full precision; other frames are composed camera-relative at render time.

## Why
- It's the right default: we **own the renderer and the physics**, so unlike
  bolt-on engines we can bake this in instead of forcing every game to architect
  around it.
- Pairs naturally with our **SDF/procedural** worlds (fractals evaluate in local
  coordinates + a frame offset) and the **fixed-step deterministic** loop.
- **Proven:** Kerbal Space Program (floating origin + "Krakensbane" + reference
  frames), Star Citizen (64-bit zones + local physics grids), Outerra; engines
  like Godot ship optional `f64` builds — we make it the default and automatic.

## Alternatives considered
- **`f32` world** — the status quo that jitters; rejected.
- **`f64` everywhere on the GPU** — GPUs are `f32`; emulated double is slow.
- **A single flat `f64` space** — still can't represent a whole galaxy at meter
  precision; hierarchical frames are required for that scale.

## Consequences
- `Transform` stores high precision (`f64` / frame + local); a derived
  **camera-relative `f32` render transform** is produced each frame.
- Rebasing is coordinated with the fixed-step loop (a quiet point between steps).
- A future **networking** layer must agree on origin/frame so peers share a space.
- Camera-relative rendering + floating origin + `f64` transforms are near-term;
  full hierarchical frames are later. Full design:
  [`../subsystems/large-world-space.md`](../subsystems/large-world-space.md).
