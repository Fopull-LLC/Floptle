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
| **shape** | Sphere, **Capsule** (capsules stand upright; best for characters), Box, or **Polygon** (2D, below). A capsule collides along its whole length, and stands on its **feet**: the ground straight beneath its centre line, so a character's feet meet a slope, a crease or a bump where they are drawn. |
| **radius / height** | Collision size |
| **bounce** | Restitution (0 = no bounce) |
| **friction** | 0 = ice, 1 = no sliding |
| **affected by gravity** | Off = floats (still collides; a script can still move it) |
| **2D** | Keep the body in the XY plane — see below |
| **freeze pos x/y/z** | Lock world-axis translation (e.g. freeze Z for 2.5D) |
| **freeze rot x/y/z** | Keep the body from tipping on an axis |

Drive a body from a script via its velocity (`node.vx/vy/vz`) rather than setting its
position — setting position fights the solver. See [scripting.md](./scripting.md#4-node--the-physics-body).

**Fast bodies.** A sphere or capsule moving more than half its radius in one
physics step (about 21 m/s for a 0.35 m capsule) sweeps its move: it stops where
it first touches a wall, loses only its speed into that wall, and keeps going
along it. So a launch, a fling or a long fall never ends up on the far side of a
mesh wall it ran into. Slower bodies move exactly as they always have. Box
and polygon bodies are not swept.

### 2D bodies

Tick **2D** and the body stays in the XY plane: it keeps the depth you gave it,
can never be pushed out of the layer, and still spins the one way a flat object
spins. That is the whole of what "this is a 2D object" means to a solver, and it
is one switch instead of working out that it means *freeze pos z, freeze rot x,
freeze rot y*.

Everything else is unchanged, and that is the point — **a 2D body collides with
the same world a 3D one does.** A tilemap's colliders, a slope you drew on a
tile, a Collidable cube, terrain: all of it works, because there is no separate
2D physics engine to be missing features. Gravity, raycasts, touch events and
layers all behave exactly as they do in 3D.

It **adds** to the freeze boxes below rather than replacing them, so ticking it
can only ever lock more, and unticking it cannot silently release an axis you
locked by hand. The axes it holds show as ticked and disabled.

A 2D box collides in its own plane: its corners and edges are tested where
it is, not half its depth in front of and behind it.

### Bodies that turn

Tick **rotates** and the physics turns the body. A contact away from its centre
spins it, so a crate pushed off a ledge tips over the edge, a ball rolls down a
slope, and a plank see-saws on a rock. The node's rotation follows. A crate
placed tilted in the editor lands tilted.

Leave it off for a character. A body that doesn't rotate keeps its orientation
however it is hit, which is what keeps a player standing up. The **freeze rot**
boxes and **2D** still hold the axes they name, so a 2D body only ever spins in
its plane.

A body added to a sprite starts with **rotates** on. A body added to anything
else, or built by a script, starts with it off.

Moving bodies collide with each other whether they rotate or not: crates stack,
and a character pushes a crate along. A layer pair the collision matrix keeps
apart, or `physics.ignorePair`, passes through.

### Polygon bodies

Pick **Polygon** as the shape and the body is an outline of up to eight corners
in its plane: a character's own silhouette, a crate with sloped sides, a round
boulder. It stands on its outline, so a character drawn from its feet up rests
with its feet on the floor, and a diamond balances on its point. Like a box, it
keeps its orientation rather than tumbling.

- **trace from art** outlines the opaque pixels of the frame the sprite is
  showing.
- Each corner has an **x** and **y** you can type. **+ corner** splits the
  longest edge, **−** removes a corner, and **box** resets to the rectangle of
  the half-extents.
- **depth** is half its thickness either side of the plane.

The outline is always kept convex. A corner dragged inward past its
neighbours drops out rather than making a dent. When the sprite flips, its
polygon body flips with it, and it scales with the node.

A **Static** polygon body is a solid wall, floor or ramp in that shape that
never moves, and it turns with the node.

From a script: `node:setBodyOutline{ vec2(-0.4, 0), vec2(0.4, 0), vec2(0, 1) }`,
for a body that changes shape with what the character is doing.

**Add Component ▸ Physics ▸ Rigidbody** on a Sprite gives it a body traced from its art: a polygon
round the opaque pixels of the frame it shows, not a box round the whole frame.
A sprite whose frame is opaque edge to edge and centred on its node gets a box
the size it is drawn, which is already exact. Either way, a falling crate needs
no further setup.

From a script: `node:getcomponent("RigidBody").two_d = 1`.

> If your 2D player falls through the level, check the **tilemap** node has
> **Collidable** on. Making a tile solid in the ◫ Tiles tab turns it on for you,
> and pressing Play warns in the console, by name, for any tilemap that has
> solid tiles and cannot collide.

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

- **Terrain** — collides against the **triangles you see**: the drawn surface, at any
  voxel size or node scale, so ground you can see under your feet is under your feet.
  Sculpt it and the collision updates. Toggle **View ▸ Terrain collider wireframe** to
  draw exactly the surface physics uses over the picture — it lies on the terrain, or
  something is wrong. The terrain node's **collide with** picker in the Inspector can
  switch a terrain to the underlying **field** instead: smoother than the picture and
  up to a fraction of a voxel away from it, but cheaper and never meshed — for a
  server that draws nothing, or a project tuned against it.
- **Mesh colliders** — check **▦ Mesh collider (walkable)** on an imported `Matter::Mesh`
  node. Its triangles (in world space) become static collision so you can walk on a map
  model. Toggle **View ▸ Mesh collider wireframes** to see them; the selected node's
  wireframe always shows. Degenerate triangles in imported meshes are filtered
  automatically.

Both coexist — a character walks across terrain and onto a mesh seamlessly.

## 5. The character controller

`scripts/first_person.lua` is a ready-made first-person controller. Attach it to a **Camera
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

`spherecast` and `capsulecast` sweep a shape the same way. A shape that starts
against a surface, such as a character standing on a floor and casting along it,
reports distance 0 at once; pass `{ ignoreStart = true }` and only a surface the
shape moves into counts:

```lua
local hit = capsulecast(node.pos, move, 0.35, 0.9, move:length(), { ignoreStart = true })
```

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

### What it costs as a scene grows

Depenetration used to test **every body against every collider**, twice per tick.
That is fine for a room and quadratic for a level: 400 bodies over 1,681 colliders
cost 4.5 s for 120 steps.

There is now a spatial index over the colliders, rebuilt once per tick, and each
body asks it which colliders can possibly reach it:

| colliders × bodies | before | after |
|---|---:|---:|
| 169 × 50 | 61.6 ms | 50.5 ms |
| 625 × 200 | 859 ms | 248 ms |
| 1,681 × 400 | 4.50 s | 556 ms |

The ratio **growing** with scene size is the point — that is a quadratic being
removed rather than a constant being shaved.

It cannot change what your game does. The index answers with *candidates* and the
same exact test runs on each, in the same order, so a scene simulates identically
— there is a test that runs the same fall against an indexed world and an
unindexed one and requires the resting position to match exactly. A collider whose
shape has no cheap bound (an infinite plane, a terrain field, a triangle mesh) is
offered to every query, exactly as before.

Nothing to configure, and nothing to opt into.
