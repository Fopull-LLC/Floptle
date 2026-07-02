# Physics & gameplay

How to make things move, fall, collide, and be walked on. This is the user-facing
guide; the design rationale lives in [subsystems/physics.md](./subsystems/physics.md)
and the ADRs.

## Contents
1. [The model in one paragraph](#1-the-model-in-one-paragraph)
2. [Rigidbodies](#2-rigidbodies)
3. [Gravity](#3-gravity)
4. [Colliders: terrain & meshes](#4-colliders-terrain--meshes)
5. [The character controller](#5-the-character-controller)
6. [Raycasting](#6-raycasting)
7. [The play loop (how it all runs)](#7-the-play-loop-how-it-all-runs)

---

## 1. The model in one paragraph

Physics runs only while **playing** (F1). On Play, the engine builds a sim from the
scene: every node with a **Rigidbody** becomes a dynamic body; the terrain and any
**mesh-collider** nodes become the static world you collide with; **Gravity Volume**
nodes define the gravity field. Each fixed timestep the sim integrates gravity, moves
the bodies, resolves penetration against the colliders, and writes the results back to
the nodes' transforms. Scripts run each frame and can read/modify a body's velocity.

## 2. Rigidbodies

Inspector ▸ **◆ Rigidbody** turns a node into a dynamic body. Properties:

| Property | Meaning |
|---|---|
| **shape** | Sphere or **Capsule** (capsules stand upright; best for characters) |
| **radius / height** | Collision size |
| **bounce** | Restitution (0 = no bounce) |
| **friction** | 0 = ice, 1 = no sliding |
| **affected by gravity** | Off = floats (still collides; a script can still move it) |
| **freeze pos x/y/z** | Lock world-axis translation (e.g. freeze Z for 2.5D) |
| **freeze rot x/y/z** | Keep the body from tipping on an axis |

Drive a body from a script via its velocity (`node.vx/vy/vz`) rather than setting its
position — setting position fights the solver. See [scripting.md](./scripting.md#4-node--the-physics-body).

## 3. Gravity

Gravity comes from **Gravity Volume** nodes (New ▸ ⬇ Gravity Volume). **A scene with no
gravity volume is zero-g** — bodies float until you add one.

| Mode | Effect |
|---|---|
| **Down** | Uniform gravity everywhere — a normal game world. `strength` = m/s². |
| **Radial** | Pulls toward the node's center within `radius` — a spherical planet. The character re-orients its "up" to the surface and can run all the way around it. |

Stack volumes for mixed worlds (a planet inside a region of down-gravity, etc.).
Per-body, **affected by gravity** opts a single body out.

## 4. Colliders: terrain & meshes

The world you collide with is built from:

- **Terrain** — collides against the *same* SDF field the renderer draws (sculpt it and
  the collision updates). Toggle **View ▸ Terrain collider wireframe** to see it.
- **Mesh colliders** — check **▦ Mesh collider (walkable)** on an imported `Matter::Mesh`
  node. Its triangles (in world space) become static collision so you can walk on a map
  model. Toggle **View ▸ Mesh collider wireframes** to see them; the selected node's
  wireframe always shows. Degenerate triangles in imported meshes are filtered
  automatically.

Both coexist — a character walks across terrain and onto a mesh seamlessly.

## 5. The character controller

`scripts/character.lua` is a ready-made first-person controller. Attach it to a **Camera
node** that has a **Capsule Rigidbody** and mark the camera active. On Play you control
that capsule (look / move / jump / run / crouch), and the camera rides it. It reads the
body's velocity, grounded state, and up vector each frame, so it works on flat worlds and
around planets without changes. Tunables (`speed`, `jump`, etc.) are editable per-node in
the Inspector. Full setup: [getting-started.md](./getting-started.md#3-add-a-player-you-control).

**Ground checks:** the built-in `node.grounded` uses a contact test (robust for the
capsule on SDF + mesh). For custom checks (step detection, edge detection, gameplay) use
`raycast` (below).

## 6. Raycasting

`raycast(ox,oy,oz, dx,dy,dz, max)` (Lua) casts a ray against the terrain + mesh colliders
and returns `{x,y,z, nx,ny,nz, distance}` or `nil`. Uses: ground checks, line-of-sight,
shooting, placing objects on a surface. It's a step-capped sphere-trace, so it's safe
against both the SDF terrain and triangle meshes; practical range is up to ~512 units.

In Rust the same is `PhysicsWorld::raycast` / `Sim::raycast`.

## 7. The play loop (how it all runs)

Each frame while playing, in order:

1. Feed each scripted body its current state (velocity, grounded, up, height).
2. Feed this frame's input; lend the colliders to scripts (so `raycast` works).
3. Run every node's `update(node, dt)`.
4. Apply the velocities + heights scripts wrote back to the bodies.
5. Step the sim on a fixed timestep (gravity → move → resolve collisions).
6. Write resolved transforms back to the nodes — **interpolated** between the last
   two fixed steps by the leftover accumulator fraction, so motion renders smooth
   at any frame rate (no whole-step aliasing); render.

The sim also runs **origin-relative** (ADR-0015): bodies and colliders use small
coordinates near a `f64` origin that follows the active camera (recentering past
4 km), while nodes and scripts always see stable world coordinates. Every static
collider — including each **terrain volume**, which gets its own collider at the
field's native resolution — is anchored on its node's `f64` translation. Content
placed millions of units out simulates as precisely as content at the origin.

Stop (F1) drops the sim and restores the scene to its pre-Play state.
