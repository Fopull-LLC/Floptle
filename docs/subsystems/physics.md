# Floptle — Physics (`floptle-physics`)

Custom, SDF-first collision: collide against the same distance function the renderer draws, so morphing fractal worlds are *interactable* without ever re-meshing.

> Decision & rationale: [`../decisions/0012-physics-sdf-first.md`](../decisions/0012-physics-sdf-first.md).
> The shared distance field & raymarch: [`./renderer.md`](./renderer.md).
> Where this sits in the engine: [`../ARCHITECTURE.md`](../ARCHITECTURE.md).

`floptle-physics` depends only on `floptle-core` (math · ECS · node facade · time).
Optional crates, pulled in *only* where noted: `parry3d` (triangle-mesh BVH
queries for imported Blender meshes), `rapier3d` (optional rigid-body dynamics).
Neither is on the critical path; the SDF world and controllers stand alone.

## Why SDF-first

Floptle renders fractals as **real, morphing worlds**; the goal is to *drive a car
across one* and *roam it on foot* **while the geometry actively shifts**. That is the
case rapier/PhysX/Jolt handle worst — they want explicit, mostly-static geometry and
choke on per-frame re-meshing.

But a fractal is already a signed-distance function `f(p, t)` we evaluate to draw it,
and an SDF is a *gift* for collision: the inside test, penetration depth, contact
normal, and continuous casts all fall out of evaluating `f` and its gradient — no
BVH, no marching cubes. Pass the current `t` and morph is free; `∂f/∂t` even lets a
character on shifting ground be **carried** by it (below). Precedent that this ships
and looks great: Media Molecule's *Dreams* (SDF rendering *and* physics) and
*Claybook* (SDF worlds you roll and deform).

## The collision world

One world per scene, rebuilt-free and queried each fixed step. It holds colliders
and a cheap broadphase; everything physics needs is expressed through one query
trait so controllers don't care whether they hit a fractal, a primitive, or a mesh.

```rust
/// Anything physics can query. Implemented by SDF, baked-field, and mesh colliders.
pub trait CollisionShape {
    /// Signed distance at world point p, at sim time t. Negative = inside.
    fn distance(&self, p: Vec3, t: f64) -> f32;

    /// Outward unit normal at p (analytic if available, else finite-difference).
    fn normal(&self, p: Vec3, t: f64) -> Vec3;

    /// Conservative world-space AABB for the broadphase (may grow with morph).
    fn aabb(&self, t: f64) -> Aabb;
}

pub struct CollisionWorld {
    colliders: Vec<Collider>,         // dense, ECS-backed (see "ECS / Node tie-in")
    broadphase: Bvh<ColliderId>,      // over collider AABBs, refit (not rebuilt) per step
    pub gravity: Vec3,
}
```

The query API every controller and the solver build on:

```rust
impl CollisionWorld {
    /// Nearest surface point + distance + normal to p. The workhorse.
    fn closest(&self, p: Vec3, t: f64) -> ClosestHit;             // { point, dist, normal }

    /// Is a sphere overlapping anything? penetration > 0 when so.
    fn sphere_overlap(&self, c: Vec3, r: f32, t: f64) -> Option<Contact>;

    /// Capsule = swept sphere over a segment a..b.
    fn capsule_overlap(&self, a: Vec3, b: Vec3, r: f32, t: f64) -> Option<Contact>;

    /// Ray vs world via sphere-marching. Robust, continuous.
    fn raycast(&self, ray: Ray, max_t: f32, t: f64) -> Option<RayHit>;

    /// Move a sphere of radius r along dir until first contact (CCD-by-construction).
    fn spherecast(&self, origin: Vec3, dir: Vec3, r: f32, max_t: f32, t: f64) -> Option<SweepHit>;

    /// Generic convex sweep (sphere/capsule) — used by the character & vehicle.
    fn sweep(&self, shape: SweepShape, motion: Vec3, t: f64) -> Option<SweepHit>;
}
```

`Contact = { point, normal, penetration }`; `SweepHit = { toi, point, normal }`
where `toi ∈ [0,1]` is the fraction of `motion` travelled before contact.

## The SDF math, concretely

**Inside / penetration.** Sign of `f` is the whole inside test. For a point:

```
f(p) > 0  → outside,  distance to surface is f(p)
f(p) = 0  → on the surface
f(p) < 0  → inside,   penetration depth = -f(p)
```

So depenetration is "push out along the normal by `-f(p)`" — no contact-manifold
bookkeeping required for the single-point case.

**Contact normal = `normalize(∇f(p))`.** Where an analytic gradient exists (the
primitives below, and many fractal folds), use it — it's exact and free. Otherwise
take the gradient by central finite differences. The cheap, robust form is the
4-tap **tetrahedron** trick (4 evals instead of 6):

```
ε = small (e.g. 1e-3 · world scale)
k1 = ( 1,-1,-1),  k2 = (-1,-1, 1),  k3 = (-1, 1,-1),  k4 = (1,1,1)
∇f ≈ normalize(
        k1·f(p + ε·k1) + k2·f(p + ε·k2)
      + k3·f(p + ε·k3) + k4·f(p + ε·k4) )
```

Choose `ε` near the field's Lipschitz scale: too small amplifies eval noise (bad on
deep fractals), too large rounds off sharp features. This is the same gradient the
renderer uses for shading normals — see [`./renderer.md`](./renderer.md).

**Casts via sphere-marching.** This is the renderer's own march, reused verbatim:
start at the origin, repeatedly step forward by the distance to the nearest surface,
because `f(p)` is exactly the radius of a guaranteed-empty sphere at `p`.

```
t = 0
loop:
    p = origin + dir * t
    d = f(p, time)
    if d < hit_eps:  return Hit { p, t, normal = ∇f(p) }   // surface reached
    t += d
    if t > max_t:    return Miss
```

For a **spherecast** of radius `r`, hit when `f(p) < r` (the swept sphere kisses the
surface) and step by `f(p) - r`. Because every step is bounded by the true distance,
this *cannot tunnel*: a bullet at any speed is caught at the first surface crossing.
Continuous collision is not a bolt-on here, it's the native behavior.

**Sphere / capsule vs SDF.** A sphere of radius `r` at center `c` overlaps when
`f(c) - r < 0`; penetration `= r - f(c)`, normal `= ∇f(c)`. A **capsule** is a
sphere swept along segment `a→b`: find the segment point nearest the surface and run
the sphere test there. In practice we sample `f` along the segment, take the minimum,
refine, and treat that as the deepest contact:

```
sphere:  pen = r - f(c);            if pen > 0: contact at c - r·n,  n = ∇f(c)
capsule: q   = argmin_{s∈[a,b]} f(s);   then sphere test at q
```

**Morph is automatic.** Every query takes the current sim time `t` and passes it
straight to `f(p, t)`. There is *no* per-frame rebuild, no dirty flag, no re-bake of
analytic colliders — the collision surface *is* the rendered surface, at the same
instant. This is the single biggest reason the system is *simpler* than a mesh
pipeline for this game, not harder.

**Surface velocity from `∂f/∂t` — the magic.** When the ground morphs under a
standing character, the surface is moving and the character should ride it. The local
surface velocity along the normal is the field's time derivative, turned from
"distance change" into "surface motion":

```
        f(p, t+dt) - f(p, t)
v_surf = - ───────────────────── · n         where n = ∇f(p)
                   dt
```

Intuition: if `f` at a fixed `p` is *increasing*, the surface is *receding* (the
point gets more "outside"), so the surface near `p` moves in `-n`; the sign and the
projection onto `n` recover that velocity. Tangential drift (sideways slide that
doesn't change distance) is invisible to `∂f/∂t` alone — fine for "stand and be
carried"; add a per-collider tangential advection field only for conveyor-belt carry.

The controller adds `v_surf` before integrating, so **a character on a shifting
fractal is carried by it** — clean here, effectively impossible to do well with a
triangle collider re-meshed every frame (old↔new vertex correspondence is lost).

```
   morphing surface at t           ... at t+dt
        ____                          ___
   ____/    \___    n↑           ____/   \__      character keeps contact,
  /            \    ●  char     /           \     inherits v_surf = -Δf/dt·n
 surface rises under the feet  → body is lifted with the ground
```

## Collider types

```
Collider ─┬─ Sdf(SdfSource)        // fractals + scene-builder primitives
          ├─ BakedField(BrickGrid) // sampled distance grid, LOD for the far field
          └─ Mesh(MeshBvh)         // static/imported Blender geometry
```

**1. Analytic SDF colliders.** Fractals (Mandelbulb/Menger/IFS folds, animated by
`t`) *and* the scene-builder primitives. The key unification: **Cube / Sphere /
Capsule / Wedge / Stairs are themselves trivial SDFs** with closed-form distance and
gradient, so they ride the exact same collision path as fractals — no special cases.

```
sdSphere(p,r)            = |p| - r
sdBox(p,b)               = |max(|p|-b,0)| + min(max(|p|-b),0)   // b = half-extents
sdCapsule(p,a,b,r)       = |p - closest_on_segment(p,a,b)| - r
// wedge = box ∩ halfspace; stairs = repeated box union (mod-space tiling)
```

**2. Baked sparse SDF / voxel field colliders.** Analytic fractals are expensive;
physics touches few points per step but we still don't want each touch to cost a deep
fractal iteration. So **bake** the analytic SDF into a grid and query *that*:

- **Generation:** sample `f(p, t)` over a sparse **brickmap** (grid of occupied
  8³/16³ bricks; empty space stored once). Skip far-from-surface bricks.
- **Query:** trilinear interpolation of the 8 corner distances for `distance`;
  gradient from the 3 axis-differences of the interpolated field (analytic, cheap).
- **Refresh cadence:** as the world morphs, re-bake bricks on a rolling budget
  (N/frame), prioritizing those near active bodies; the field lags the analytic
  surface by a frame or two — invisible at gameplay scale.
- **Near/far LOD:** **analytic** SDF in a radius around the player (exact contact
  where it matters), **baked** field everywhere else; the `CollisionWorld` picks
  per-query by distance to active bodies.

This is the primary performance mitigation — it decouples physics cost from raymarch
cost (precedent: *Dreams*, *Claybook*).

**3. Triangle-mesh BVH colliders.** Imported Blender meshes are static/rigid, so a
**one-time** BVH is cheap and never rebuilt. Use a small custom BVH or `parry3d` for
the closest-point / raycast / sweep queries. These coexist with SDF colliders in the
same world behind `CollisionShape`; a mesh just answers `distance`/`normal` via its
BVH instead of a field. (No re-meshing here — that's the whole point of staying SDF
for the things that *do* deform.)

## Kinematic character controller (capsule)

A capsule, moved by **move-and-slide** built on `spherecast`/`sweep` — no rigid-body
solver involved. Per fixed step:

```
1. desired = (input_dir · speed + inherited_surface_v) · dt
2. for up to K iterations:
     hit = world.sweep(capsule, remaining_motion, t)
     if none: move full remaining_motion; break
     move to hit.toi; slide: remaining -= (remaining · hit.normal) · hit.normal
3. depenetrate: while (pen = world.capsule_overlap(...)) > 0:
     move += pen.normal * pen.penetration          // push out along ∇f
4. ground check: short spherecast down; grounded if hit & slope ≤ slope_limit
5. inherit v_surf from the ground contact (see ∂f/∂t above) for next step
```

- **Ground & slopes:** `grounded` when the down-cast hits within a small probe and
  the surface normal's angle to up `≤ slope_limit` (e.g. 50°). Steeper → slide.
- **Steps:** to climb a `≤ step_height` ledge, try an up-cast → forward-cast →
  down-cast (the classic "step offset") so stairs and fractal terraces are walkable.
- **Depenetration along the gradient:** because the normal is `∇f`, pushing out by
  `-f(p)` is the shortest exit — robust even as the surface closes in during a morph.
- **Carried by morphing ground:** step 5 is the "roam a shifting fractal" goal —
  `v_surf` is folded into `desired` next step, so standing still on rising terrain
  lifts the player with it.

## Raycast vehicle

A rigid chassis body with **N raycast (spherecast) wheels** — no wheel colliders, no
mesh contact. Each wheel casts **downward** along the suspension axis against the SDF
world:

```
for each wheel:
    hit = world.spherecast(wheel_pos, -chassis_up, wheel_r, rest+travel, t)
    if hit:
        compression = rest - hit.toi_distance
        F_spring  = k · compression
        F_damp    = c · (compression - prev_compression)/dt
        F_susp    = (F_spring + F_damp) · hit.normal       // along ground normal
        // tire forces in the contact plane:
        F_long    = engine/brake torque mapped through grip
        F_lat     = -lateral_slip · grip                   // resist sideways slide
        apply (F_susp + F_long + F_lat) at hit.point to the chassis
```

The chassis integrates as one rigid body (position + orientation + linear/angular
velocity). Because wheels probe via sphere-marching, the car **drives on a living,
morphing fractal**: each frame the casts find the current surface, and `∂f/∂t` at the
contacts lets rising terrain push the wheels up naturally. The "drive a car across
one" goal.

## Optional rigid-body dynamics (object-vs-object)

Stacking crates and joints are **out of the launch core** and added only when a game
needs them. Two paths, the world/controllers depend on neither:

- **Lightweight sequential-impulse solver (custom).** Generate contacts straight
  from SDF queries: for each body pair (and body-vs-world), `sphere_overlap` /
  `capsule_overlap` give `{ point, normal, penetration }`. Then run sequential
  impulses over a few iterations:

  ```
  for iter in 0..solver_iters:
    for c in contacts:
      v_rel   = dot(relative_velocity_at(c.point), c.normal)
      lambda  = -(1+e)·v_rel / effective_mass(c)         // e = restitution
      apply ±lambda·c.normal as impulses to the two bodies
    // + Baumgarte/position bias to resolve c.penetration without adding energy
  ```

- **Delegate to `rapier3d`** for full-featured stacking/joints, while the **world
  stays SDF**: feed rapier static colliders sampled from the SDF near dynamic bodies,
  or run rapier bodies and resolve them against the SDF world via the same contact
  generation. Bolt-on, not a dependency of the character/vehicle.

## Stepping & determinism

Physics runs in the **fixed-timestep** stage of the frame loop (see
[`../ARCHITECTURE.md`](../ARCHITECTURE.md) §3), driven by an accumulator:

```
accumulator += frame_dt
while accumulator >= FIXED_DT:
    physics.step(FIXED_DT)        // sub-stepped further for fast bodies
    accumulator -= FIXED_DT
alpha = accumulator / FIXED_DT    // render interpolates poses by alpha
```

- **Sub-stepping:** split `FIXED_DT` for very fast bodies; sphere-marching already
  prevents tunneling, so sub-steps are about solver/force stability, not CCD.
- **Determinism:** a custom fixed-step, fixed-iteration solver with a deterministic
  collider iteration order is reproducible bit-for-bit on a platform — important for
  game-feel and the **future networking** goal (authoritative fixed-step sim,
  serializable state; [`../ARCHITECTURE.md`](../ARCHITECTURE.md) §10). A reason to own
  the solver rather than inherit a third party's float ordering.

## Performance

- **Budget asymmetry:** the renderer evaluates `f` for *millions of pixels*; physics
  for *a few bodies × a few taps* per step. Even costly analytic fractals are
  affordable at that count — the only danger is very deep iteration counts.
- **Baked field** is the main mitigation: queries hit a trilinear grid, not the
  analytic fold (analytic only in the near radius around the player).
- **Caching:** memoize `f`/`∇f` per body per step (the controller queries the same
  neighborhood repeatedly); reuse the renderer's eval at a shared point/time.
- **Broadphase:** a BVH/grid over collider AABBs, **refit** (not rebuilt) each step;
  narrowphase SDF evals run only for colliders whose AABB overlaps the query.

## ECS / Node tie-in

Physics is just another ECS system over a **`Collider` component** (ADR-0005's
Node/Component facade, [`../ARCHITECTURE.md`](../ARCHITECTURE.md) §2):

```rust
struct Collider {
    source: ColliderSource,   // Sdf(handle) | BakedField(handle) | Mesh(asset_id)
    kind:   Body,             // Static | Character | Vehicle | Dynamic
    collidable: bool,         // the scene-builder "collidable" toggle
}
```

- A `Collider` referencing an **SDF source** points at the *same* `f(p, t)` the
  renderer's raymarch pass draws — one source of truth for shape and collision.
- Scene-builder shapes (Cube/Sphere/Capsule/Wedge/Stairs) expose a **collidable
  toggle**; flipping it on inserts/removes the `Collider` component — no geometry
  conversion, since the primitive already *is* an SDF.
- Everything is **RON**-serialized like the rest of the project, so collider setup
  is diffable and hand/AI-editable.

## Out of scope at launch

- A full general-purpose **rigid-body sandbox** (rich stacking, joints, motors as a
  first-class feature) — the optional solver covers incidental dynamics only.
- **Soft-body / cloth** simulation.

Both can layer on later behind the same `CollisionWorld`; nothing in the SDF world or
the character/vehicle controllers needs to change to add them.
