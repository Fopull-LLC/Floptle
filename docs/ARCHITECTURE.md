# Floptle — Architecture

> How the pieces fit. This is the map; the detailed designs live in
> [`subsystems/`](subsystems/) and the *why* of each major choice lives in
> [`decisions/`](decisions/).

## 1. Layering

Floptle is a Rust workspace of small crates arranged in dependency layers. Lower
layers never depend on higher ones. This keeps the engine modular, fast to
compile, and easy to reason about solo.

```
        ┌─────────────────────────────────────────────┐
  app   │   floptle-editor (bin)   floptle-runtime (bin)│
        └───────────────┬───────────────────┬───────────┘
                        │                   │
        ┌───────────────┴───────────────────┴───────────┐
  feat  │ vfx·anim·physics·matter·ui·assets·script·input·net│
        └───────────────┬───────────────────┬───────────┘
                        │                   │
        ┌───────────────┴───────┐   ┌────────┴───────────┐
  mid   │   floptle-render      │   │  floptle-shader     │
        └───────────────┬───────┘   └────────┬───────────┘
                        │                    │
        ┌───────────────┴───────┐            │
  geo   │   floptle-field       │  (SDFs + CSG/blend ops; shared by render,
        │  implicit geometry    │   physics, and matter — ADR-0013)
        └───────────────┬───────┘
                        │
        ┌───────────────┴────────────────────────────────┐
  base  │                floptle-core                      │
        │   math · ECS · node facade · events · time · pool│
        └─────────────────────────────────────────────────┘
```

- **core** depends on nothing engine-specific (only `glam`, `serde`, etc.).
- **render** and **shader** sit on core. render uses shader's compiled output.
- **feature crates** compose render/shader/core into systems.
- **editor** and **runtime** are the only binaries; they wire systems into an app.

## 2. The data model: ECS core, Node facade on top

This is the central architectural decision (ADR-0005). Two views of the same data:

- **Authoring view — Nodes & Components.** What you manipulate in the editor and
  scripts: a tree of **nodes**, each with a transform, optional components
  (MeshRenderer, ParticleEmitter, Camera, UIElement, Collider, …) and optional
  **scripts**. This is the familiar Unity/Godot mental model you asked for.
- **Runtime view — ECS.** Under the hood, nodes/components are stored in a
  data-oriented **archetype ECS**: components packed in contiguous arrays,
  systems iterating tightly for cache-friendly, hyperoptimized updates.

The node tree is a *facade*: a Node is an entity id + a hierarchy/transform
component; "adding a component" inserts a component into the ECS; a script is a
component holding a Lua reference. You get ergonomic authoring **and** DOD speed.

```
Node "Player"                    ECS
 ├─ Transform           ⇄        Entity 42: [Transform][MeshRenderer][Script][Collider]
 ├─ MeshRenderer
 ├─ Script "player.lua"
 └─ Collider
```

## 3. Frame loop & scheduling

A single authoritative frame loop in the runtime/editor:

```
poll OS events (winit) ──▶ input.update (actions)
        │
        ▼
fixed-step accumulator ──▶ scripts.on_fixed_update + physics step(s)   (deterministic)
        │
        ▼
variable update ────────▶ scripts.on_update ─▶ anim ─▶ vfx sim ─▶ ui
        │
        ▼
render: build render graph ─▶ submit to wgpu ─▶ present
```

- **Fixed timestep** for gameplay/physics determinism (and future networking).
- **Variable timestep** for animation/VFX/render interpolation.
- Threading starts simple (main-thread sim, render submission), with the ECS and
  render graph designed so heavy systems (vfx sim, culling) can move to a job
  pool later **without** reshaping gameplay code.

## 3b. Large-world space (default-on)

The world moves around the player, not the other way around — automatically, with
no developer work (ADR-0015). `f32` jitters far from the origin and GPUs are
`f32`-native, so Floptle keeps the active simulation near `(0,0,0)` and treats
space as large-world-safe by default in three always-on layers (plus one for
galaxy scale):

- **Camera-relative rendering** — positions are uploaded relative to the camera
  (model-view formed in `f64`, cast to `f32` last); the GPU never sees big numbers.
- **Floating origin** — when the camera passes a threshold, the world is **rebased**
  by the offset between fixed steps (positions shift; velocities are
  translation-invariant, so they don't), so physics never sees large coordinates.
- **`f64` authoritative transforms** — `Transform` is double precision; a derived
  camera-relative `f32` render transform is produced each frame.
- **Hierarchical reference frames** *(galaxy+)* — nested frames (galaxy → system →
  body → local); only the player's local frame is simulated at full precision.

This composes with the rest of the engine: SDFs/fractals evaluate in **local**
coordinates + a frame offset (so "infinitely deep/far" stays precise), the
fixed-step loop stays deterministic (rebase at a defined point), and the
player's frame can double as the gravity-aligned frame (§9d). Full design:
[`subsystems/large-world-space.md`](subsystems/large-world-space.md).

## 4. Rendering pipeline (overview)

`floptle-render` owns a **render graph**: passes declare their resource reads/
writes and the graph orders/aliases them. The signature look is built from:

1. **Raster pass** for ordinary meshes (Blender imports, procedural shapes).
2. **Raymarch/SDF pass** for fractals and volumetric/impossible geometry — the
   "fly inside a fractal" capability. Runs full-screen or in bounded volumes.
3. **Material/shader binding** from `floptle-shader` compiled IR → WGSL pipelines.
4. **Post stack** — screen-space passes that intentionally break lighting rules
   (feedback/echo, non-physical color transport, spatial warps) for the surreal,
   dreamlike, nostalgic-underneath look.

Backends are abstracted by **wgpu** (Vulkan/Metal/DX12/GL), so the same renderer
runs on all three OSes. See [`subsystems/renderer.md`](subsystems/renderer.md).

## 5. Shaders as a first-class IR

`floptle-shader` holds one **IR** that is simultaneously a node graph and a text
format (`.flsl`). The editor edits the graph; "Open in VSCode" prints the IR as
text; saving either re-parses into the IR; the IR transpiles to WGSL (validated
by naga). This single-source-of-truth is what lets graph and text stay in sync.
See [`subsystems/shaders.md`](subsystems/shaders.md) and ADR-0007.

## 6. Assets & the Blender pipeline

Blender → **glTF 2.0** → `floptle-assets` import (meshes, UVs, materials, skins,
animations). An **asset database** assigns stable ids, watches files for hot
reload, and tracks dependencies (which materials use which textures/shaders).
Textures carry tiling metadata (repeat/clamp/flip/count) so "drag on and tile"
needs no shader. See [`subsystems/asset-pipeline.md`](subsystems/asset-pipeline.md)
and [`subsystems/materials-and-textures.md`](subsystems/materials-and-textures.md).

## 7. Scripting & hot reload

`floptle-script` hosts **Lua (LuaJIT via mlua)**. Each Script component binds a
`.lua` file and gets lifecycle callbacks (`on_ready`, `on_update`,
`on_fixed_update`, `on_event`). The engine exposes a curated, safe API (node
access, input actions, events, pools, vfx spawning). File-watching gives
hot-reload. Scripts open in VSCode rooted at the project. See ADR-0003 / 0011.

## 8. Serialization & project format

Everything authored is **RON** (Rusty Object Notation): scenes, prefabs,
materials, particle effects, shader graphs, input maps. RON is human-readable
and diff-friendly, so projects play nicely with git and can be hand-edited or
AI-edited in VSCode. A Floptle **project** is a directory of these files +
`assets/` + Lua scripts; an **export** packs a project with `floptle-runtime`.

```
MyGame/
├─ project.ron            # project settings, entry scene
├─ scenes/*.ron
├─ prefabs/*.ron
├─ materials/*.ron
├─ vfx/*.ron              # particle effects (timeline + groups + curves)
├─ shaders/*.flsl         # textual shader IR (also editable as graphs)
├─ scripts/*.lua
└─ assets/                # textures, glTF models
```

## 9. Threading & performance posture

- **Default-fast:** dependencies always built with `opt-level=3`; release builds
  use thin-LTO + single codegen-unit + panic=abort + strip.
- **Data-oriented:** hot data in contiguous arrays; avoid per-entity heap churn.
- **Pools everywhere churn happens:** the engine-native pool (ADR-0008) backs
  particles, transient nodes, and gameplay spawns.
- **Measure:** a lightweight in-engine frame profiler is part of the editor from
  early on — "lightweight & fast" is verified, not assumed.

## 9b. Physics: SDF-first, because the worlds morph

Floptle's signature gameplay need — driving, rolling, and roaming on **fractals
that are actively morphing** — is precisely what off-the-shelf rigid-body
engines handle *worst*: they assume explicit, mostly-static collision geometry
and choke on per-frame re-meshing. So `floptle-physics` owns the collision core
and makes it **SDF-first**: it collides against the same signed-distance
function the renderer draws (`f(p, t)`), which is both cheaper and more robust
than meshing a deforming surface every frame.

Why this is the *simpler* path here, not the harder one:

- **Inside test / penetration** = sign and magnitude of `f(p)` — no BVH, no re-mesh.
- **Contact normal** = `normalize(∇f(p))` via a few finite-difference evals.
- **Sphere/capsule/ray casts** = sphere-marching — literally what the renderer
  already does — giving robust *continuous* collision for free.
- **Morph is automatic**: evaluate `f(p, t)`; the collider tracks the animation.
- **Riders inherit the surface**: surface velocity from `∂f/∂t` lets a character
  standing on a shifting fractal be carried by it — clean here, near-impossible
  with a re-meshed triangle collider.
- **Analytic primitives are just SDFs too**, so the scene-builder's Cube/Sphere/
  Capsule/Wedge/Stairs share the exact same collision path as the fractals.

Layered so we own the novel parts and borrow only the boring ones:

1. **SDF collision world** (custom) — fractals + analytic primitives.
2. **Baked sparse SDF/voxel field** (custom) — decouples physics cost from the
   expensive analytic fractal (analytic near the player, baked grid farther out;
   precedent: Media Molecule's *Dreams*, *Claybook*).
3. **Triangle-BVH colliders** for static/imported Blender meshes (small custom
   BVH, or `parry3d` for the queries) — these are static/rigid, so a one-time BVH
   is cheap.
4. **Kinematic character + raycast-vehicle controllers** (custom) — movement feel
   is gameplay-critical; you want full authorship anyway.
5. **Optional rigid-body dynamics** for object-vs-object stacking/joints — added
   only when a game needs it (lightweight custom impulse solver, or `rapier3d`).

Fixed-timestep stepping keeps it deterministic — good for game-feel and for the
future networking goal. Full design: [`subsystems/physics.md`](subsystems/physics.md),
rationale in ADR-0012.

## 9c. Deformable matter: one substrate, opt-in tiers

The engine's most distinctive system. The premise: **nothing is forced to be a
static box** — any object (fractal or developer-built shape) can declare how it
behaves *as matter*. This is unified by the **`floptle-field`** layer: all
geometry can be expressed as an implicit field `f(p, t)`, so:

- combining shapes is **algebra** — `smin`/`smax` with a blend radius `k` give
  clean **merge** ("soup"), **mix**, or **reject** per a material interaction
  matrix; and
- the *same field the renderer draws is the field physics collides against*, so a
  morphing/blending/soft object is automatically renderable **and** collidable —
  no separate collider to keep in sync.

**`floptle-matter`** adds a `MatterModel` component with cost-tiered, opt-in
behavior (you pay only for what you reach for):

```
Rigid  →  Morph        →  FieldBlend    →  SoftBody     →  Viscoelastic
(free)    GPU vertex/      CSG soup/mix/    XPBD particle   adhesion (stick),
          field displace   reject (smin)    cage + mesh     stretch, fracture
                                                            into strands (future)
```

Data flow — deformable objects write into a **shared sparse distance field
(brickmap)** that both the renderer and physics read:

```
MatterModel ─▶ solver(tier) ─▶ updated field / vertex buffers
                                    │            │
                                    ▼            ▼
                           floptle-render   floptle-physics
                           (raymarch/mesh)  (collide f(p,t))
```

Real techniques behind it (so they're researchable): SDF CSG; polynomial
smooth-min; generalized winding number for mesh→SDF; surface nets / dual
contouring for field→mesh; XPBD; shape matching; MLS-MPM/APIC as the heavyweight
"true goo" option; elastoplasticity + damage for fracture. Precedents: *Dreams*,
*Claybook*. Near-term ships Morph + FieldBlend + basic XPBD + simple adhesion;
full MPM soup and topological fracture are later/research. Full design + math:
[`subsystems/deformable-matter.md`](subsystems/deformable-matter.md), ADR-0013.

## 9d. Gravity & density: matter that pulls

Gravity is not a constant — it's a composable **vector field** `g(p)` a body
samples as "down" (ADR-0014). Because `floptle-field` carries spatial scalar/
vector fields (not just SDFs), density `ρ(p)` and the gravity it produces live in
the same substrate as geometry. Contributions compose, cheapest → heaviest:

```
g(p) = g_global  +  Σ g_source(planets)  +  g_sdfSurface(-∇f)  +  g_densityField(-∇Φ)
        constant     Newtonian -GM r̂/r²    grounds you on a       Poisson ∇²Φ=4πGρ,
        /volume                              fractal's walls        Barnes-Hut / FFT
```

The kinematic character controller (`floptle-physics::character`) samples `g(p)`,
aligns its up to `-g`, and move-and-slides in that frame — so **running on a
fractal and up its swirling walls**, **walking on procedural planets**, and
**spaceship free-flight** are all the same mechanism at different tiers. Density is
also a physical material property in `floptle-matter`: it sets **mass** (`m=ρ·V`,
→ inertia + gravity emission) and, with **bulk modulus + yield**, whether matter
**crushes or resists** (ADR-0013). The `gravity` solver is a module in
`floptle-physics`, reading density from `floptle-field` — `matter` writes density,
so there is no dependency cycle. Tiers 0–2 are near-term; the full calculated
density-field tier for huge/infinite worlds is later. Full design:
[`subsystems/gravity-and-density.md`](subsystems/gravity-and-density.md).

## 10. Networking boundary (deferred)

`floptle-net` exists now only to *hold a seam*. Gameplay logic is written against
an authoritative-update model (fixed timestep, serializable component state) so a
dedicated **server build** + connecting clients can be added later on the maker's
own infrastructure — without reshaping the engine. Not a launch requirement.
See [`subsystems/networking-future.md`](subsystems/networking-future.md).

## 11. Crate dependency rules (enforced by review)

- No upward dependencies (a base crate never imports a feature crate).
- `floptle-core` stays free of wgpu/winit/egui — it's pure engine data + logic.
- `floptle-field` (implicit geometry) sits just above core and is the **shared**
  representation: `render`, `physics`, and `matter` depend on it, never vice-versa.
- `floptle-matter` depends on `core` + `field` + `physics`; nothing depends on
  `matter` except the binaries — so the deformation system is fully removable.
- Only `editor` and `runtime` may depend on the windowing/event-loop entrypoint.
- Each subsystem is independently testable with its data types in isolation.
