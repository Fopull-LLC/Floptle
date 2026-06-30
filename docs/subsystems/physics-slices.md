# Physics — implementation slices

The design lives in [`physics.md`](./physics.md) + ADRs [`0012-physics-sdf-first`](../decisions/0012-physics-sdf-first.md)
and [`0014-gravity-fields`](../decisions/0014-gravity-fields.md). This file slices
that design into committed, independently-shippable increments. Slices 1–2 are pure
`floptle-physics` and fully headless-unit-testable; 3+ touch the editor/ECS and need
playtesting. The defining bets: **SDF-first collision** (collide against the same
`f(p,t)` the renderer draws — no re-meshing of morphing fractals) and a **composable
gravity field** (so Mario-Galaxy spherical-planet worlds are out-of-the-box).

## Slice 1 — Collision core + integrator  ✅ (this commit)
`floptle-physics`, no editor. Foundational vertical slice that proves SDF-first + gravity.
- `CollisionShape` trait (`distance(p,t)` + `normal(p,t)`); `SdfTerrain` collider wrapping
  `floptle_field::Terrain` (distance via `sample`, normal via `normal`); an analytic
  `Plane` + `Sphere` collider for tests.
- `GravityField` = a sum of composable sources (ADR-0014 tiers): `Uniform(vec)` and
  `SdfSurface(-∇f)` (pulls a body onto a fractal/planet surface).
- `PhysicsWorld` holds colliders + dynamic `Body`s (sphere collider, pos/vel/mass,
  restitution/friction). Fixed-timestep `step(dt)`: integrate gravity → resolve
  sphere-vs-shape penetration (push out along the contact normal, cancel into-surface
  velocity, apply restitution + friction), with a substep cap.
- **Acceptance (unit tests):** a sphere dropped above flat SDF terrain settles at rest
  on the surface; on a tilted plane it slides downhill and accelerates; with radial
  `SdfSurface` gravity around a sphere world a body falls *inward* to the surface from
  any side; no energy blow-up over many steps.

## Slice 2 — Kinematic character controller  ✅
`Character` capsule controller (the "cool movement"): move input + gravity + ground
snap + slope limit; **orients "up" to −gravity** so it runs around spherical planets and
up swirling fractal walls (ADR-0014). Capsule-vs-SDF (both end-spheres) via the same
trait. **Tests pass:** walks flat ground without sinking; **circumnavigates a
sphere-planet** under radial gravity staying grounded + upright (Mario Galaxy on foot);
respects the slope limit (gentle = grounded, steep = slides).

## Slice 3 — Editor + ECS + play integration
- Components on entities: `RigidBody { mass, … }` and `Collider { shape, kind: Solid|Trigger }`
  (shape = sphere/capsule/box/auto-from-terrain). Serialize via `MatterDoc`/a sidecar like
  materials. Toggle Solid vs Trigger in the inspector; collider gizmos.
- On **Play**: build a `PhysicsWorld` from the scene (terrain → `SdfTerrain` collider,
  bodies from `RigidBody`+`Collider`), step on a fixed-timestep accumulator decoupled from
  render fps, write resolved transforms back to the ECS; restore on Stop.
- Gravity **volume nodes** (`Uniform` / `Point`(planet) / `SdfSurface`) feed the field; a
  body samples the summed field each step → spherical-planet gameplay with zero code.
- **Acceptance (playtest):** drop a ball in a scene, press Play, it falls and rolls on the
  sculpted terrain; a planet node makes objects orbit/stick.

## Slice 4 — Lua physics API + triggers + mesh colliders
- Lua: `body.velocity`, `apply_impulse`, `is_grounded`, and `on_collision` /
  `on_trigger_enter`/`exit` callbacks (builds on the existing input API).
- Trigger volumes fire enter/stay/exit events (no resolution).
- Static **mesh colliders** for imported glTF (parry3d BVH) so non-SDF geometry collides.

## Slice 5 — Vehicle + gravity tooling (later/research)
- Raycast-vehicle model (drive across a fractal).
- Density-field/Poisson gravity tier (ADR-0014 `DensityField`) + gravity-field arrow viz.

Memory rule: commit under Ty Johnston, no co-author; build with the sandbox disabled;
verify each headless slice with `cargo test -p floptle-physics`.
