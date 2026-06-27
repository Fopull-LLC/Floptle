# ADR-0012 — Physics: a custom, SDF-first collision core

- **Status:** Accepted · 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
Floptle renders **fractals as real, morphing worlds** and wants players to
*interact* with them — driving a car across one, roaming with a movement system —
**while the geometry actively shifts**. This is the exact scenario off-the-shelf
rigid-body engines (rapier, PhysX, Jolt) handle worst: they assume explicit,
mostly-static collision geometry (convex hulls, baked triangle-mesh BVHs) and
choke when the surface re-meshes every frame.

Crucially, our fractals are already defined as **signed-distance functions**
`f(p, t)` for rendering. An SDF is a *gift* for collision, not an obstacle.

## Decision
Own the collision core in `floptle-physics`, **SDF-first**: collide against the
same distance function the renderer draws. Layer it so we build the novel parts
and borrow only the boring ones:

1. **SDF collision world** (custom) — fractals + analytic primitives.
2. **Baked sparse SDF/voxel field** (custom) — query a cheap distance grid for
   physics, refreshed as the world morphs; analytic near the player, baked far.
3. **Triangle-BVH colliders** for static/imported Blender meshes (small custom
   BVH, or `parry3d` for queries) — static, so a one-time BVH is cheap.
4. **Kinematic character + raycast-vehicle controllers** (custom) — movement feel.
5. **Optional rigid-body dynamics** for object-vs-object — added only if a game
   needs it (lightweight custom impulse solver, or `rapier3d`).

## Why this is the *simpler*, faster path here
- **Inside / penetration** = sign & magnitude of `f(p)` — no BVH, no re-meshing.
- **Contact normal** = `normalize(∇f(p))` from a few finite-difference evals.
- **Casts** (ray/sphere/capsule) = sphere-marching — what the renderer already does
  — giving robust *continuous* collision essentially for free.
- **Morphing is automatic**: evaluate `f(p, t)`; collision tracks the animation.
- **Riders inherit the surface**: `∂f/∂t` yields surface velocity, so a character
  on a shifting fractal is carried by it — clean here, near-impossible with a
  re-meshed triangle collider.
- **Unification**: scene-builder primitives (Cube/Sphere/Capsule/Wedge/Stairs)
  are themselves trivial SDFs, so they share the fractal collision path.
- **Determinism**: a fixed-step custom solver is easy to keep deterministic —
  good for game-feel and the future networking goal.

## Precedent
SDF-native engines ship and produce unique looks: **Media Molecule's *Dreams***
(SDF rendering *and* physics) and **Claybook** (SDF worlds with rolling/deforming
terrain). This validates both feasibility and the on-brand visual payoff.

## Alternatives considered
- **rapier/PhysX/Jolt with triangle-mesh colliders** — would require re-meshing
  (marching cubes) and rebuilding a BVH every frame for morphing fractals:
  expensive, lossy, and fragile. Rejected for the core need.
- **Marching cubes → rigid body** — adds a heavy meshing step and quantizes the
  surface; the SDF already has exact distance/normal info we'd be throwing away.

## Consequences
- We implement collision queries and the character/vehicle controllers ourselves
  — meaningful work, but it *is* the distinctive value (and simpler than fighting
  a mesh engine here).
- Analytic fractal SDFs are costly to evaluate; mitigated by the baked sparse
  field (decouples physics cost from raymarch cost) and near/far LOD.
- Full design and math: [`../subsystems/physics.md`](../subsystems/physics.md).
