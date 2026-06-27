# Floptle — Roadmap

> A phased plan from empty scaffold to "a maker can ship a small game." Each
> phase ends with a **demo you can actually run** — we never build subsystems in
> the dark. Order favors getting *pixels and iteration* early, then layering the
> headline features, then polish/export.
>
> This is intentionally not date-bound (solo project). It's ordered by dependency
> and by "what unblocks the most fun next." Reorder freely as priorities shift.

## Guiding principles for sequencing

1. **Visible early.** A window + triangle + camera before anything abstract.
2. **Vertical slices.** Each phase touches the whole stack thinly rather than
   finishing one crate perfectly.
3. **Dogfood.** The editor uses the engine; building the editor pressure-tests it.
4. **De-risk the THESIS, not the tooling.** The raymarch renderer carries the
   entire "another dimension" payload, so prototype it immediately. The shader IR
   is *tooling* that produces no new visuals over hand-written WGSL — defer it
   until you feel the pain of editing WGSL by hand.

> **The proof comes first.** The single highest-leverage build is the
> **"Am I Dreaming?" slice**: a hardcoded-WGSL raymarch + post flythrough of a
> morphing fractal (Phases 1 & 4 collapsed into one tiny binary), GPU profiler on
> from day one. It is the smallest "holy crap" demo *and* the go/no-go gate for the
> whole bet — if a stranger doesn't ask "what *is* that," you learn it in weeks, not
> years. Build it before breadth, then let one small game pull the rest of the
> phases into existence on demand. (Synthesized from the vision advisory pass.)

---

## Phase 0 — Foundations *(this repo, now)*
**Goal:** a real, compiling skeleton and a complete written design.

- [x] Cargo workspace + crate split + tooling config.
- [x] Vision, architecture, roadmap, ADRs, subsystem design docs.
- [ ] CI: `cargo fmt --check`, `clippy -D warnings`, `cargo build` on Lin/Win/Mac.
- [ ] Decide wgpu vs. ash *(recommended: wgpu — ADR-0002, awaiting final sign-off)*.

**Demo:** `cargo run` prints the banner. Docs read top-to-bottom and cohere.

## Phase 1 — Window, viewport, camera, core loop
**Goal:** something on screen and a frame loop to hang everything on.

- winit window + wgpu device/surface bootstrap (`floptle-render::device`).
- Frame loop with fixed + variable timestep (`floptle-core::time`).
- Clear-color → first triangle → textured quad.
- Minimal archetype ECS + Transform + a free-fly debug camera.
- **Large-world foundations, on by default** (ADR-0015): `f64` transforms +
  camera-relative rendering + floating origin, so far-from-origin never jitters.
- In-engine frame profiler overlay (lightweight, stays forever).

**Demo:** fly a camera around a spinning textured quad at a locked frame rate.

## Phase 2 — Meshes, materials, the Blender pipeline
**Goal:** get *your* art in.

- glTF import: meshes, UVs, normals, base materials (`floptle-assets`).
- Mesh upload + a basic raster pass + simple lighting (`floptle-render`).
- Material asset (RON) binding a shader + textures + params.
- Texture tiling metadata (repeat/clamp/flip/count) — "drag on and tile".
- Asset database with hot-reload of textures/models.

**Demo:** export a Blender model, drop it in, see it lit and textured; tweak a
tiling value and watch it update live.

## Phase 3 — The shader IR (headliner #1)
**Goal:** de-risk the signature shader system early.

- Shader IR data model + stdlib nodes (noise/sdf/color/warp) (`floptle-shader`).
- IR → WGSL transpile via naga; hook into the material system.
- `.flsl` text format: round-trippable parse/print; "Open in VSCode" (ADR-0011).
- Minimal node-graph view in the editor (graph ⇄ IR).

**Demo:** build a trippy material in the graph, open it as `.flsl` text, tweak a
line, switch back — same shader, both ways.

## Phase 4 — The otherworldly renderer (headliner #2)
**Goal:** the "from another dimension" look and fractals you fly into.

- Render graph with declared passes/resources.
- SDF/raymarch pass; a fractal (Mandelbulb/Mandelbox/menger) you can enter.
- **`floptle-field`** shared substrate (SDF + CSG `smin`/`smax`) online; the
  first deformable-matter tiers — **Morph** (GPU vertex/field displacement) and
  **FieldBlend** (blend/mix/reject "soup") — land here (ADR-0013).
- **Programmable light**, first tier (ADR-0016): bent-ray transport in the march
  (`bend = -∇f`, then `bend = g(p)` so light falls under your gravity) — off by
  default; the cheap headline that proves rules compose.
- Screen-space post stack (feedback/echo, non-physical color, spatial warp).

**Demo:** fly inside a fractal that morphs in real time, with a post stack that
makes it feel impossible.

## Phase 5 — Scripting & the node/scene model
**Goal:** make it programmable and saveable.

- Lua host + lifecycle + curated engine API (`floptle-script`).
- Node/Component facade over the ECS; scene (de)serialization to RON.
- **Lawset/Realm thin seam** (ADR-0018): a `lawset.ron` + resolver wiring just two
  axes (gravity-as-realm-default + `time.scale`) — so the spine exists before
  systems calcify, without a big-bang. Proof: cross a `smin` boundary and watch
  "down" *and* time-rate change together.
- Hot-reload of scripts; VSCode "open script" integration end-to-end.
- Input action mapping (kbd/mouse/gamepad) scripts can query (`floptle-input`).

**Demo:** attach `player.lua` to a node, bind WASD+gamepad to `Move`, drive a
character; edit the script and see it hot-reload.

## Phase 6 — Scene building & shapes in the editor
**Goal:** build levels *in* the scene view.

- egui + egui_dock editor shell; dark/retro/high-contrast theme (ADR-0004).
- Scene hierarchy, inspector, gizmos (move/rotate/scale), pick/place.
- Procedural shapes: Cube/Sphere/Cylinder/Capsule/Wedge/Stairs(step count),
  draw-base-then-pull-height; editable post-creation; collidable toggle.
- **SDF collision world** (`floptle-physics`) — analytic primitives + triangle-
  BVH for imported meshes; kinematic capsule character controller. The Phase-4
  fractal SDF plugs into this *same* world, so you collide with morphing
  fractals via the renderer's own distance function (ADR-0012).
- **Field gravity** (`floptle-physics::gravity`) tiers 0–2: global/volume,
  analytic sources (planets), and **SDF-surface gravity** `g ∝ -∇f`. The
  controller aligns "up" to `-g`, so you can **run on a fractal and up its
  swirling walls** (ADR-0014).

**Demo:** right-click → build a staircase and some shapes, texture them, mark
them collidable, walk on them with the Phase-5 character — then walk straight up
a vertical fractal wall because its surface gravity keeps you grounded.

## Phase 7 — The VFX timeline (headliner #3)
**Goal:** the particle editor you actually want.

- ParticleEffect (name/lifetime/loop|oneshot/end-behavior) (`floptle-vfx`).
- Timeline UI with tracks + `Emit` events (auto-rate **and** hand-placed).
- Particle groups + property set; value-or-curve properties with the graph editor.
- Data-oriented (GPU-friendly) particle sim; render integration.

**Demo:** build "360Slash" with a Crescents group and a Smoke group, curve their
size/rotation/alpha/color over life, scrub the timeline like a video editor.

## Phase 8 — Animation, cameras, dialogue, UI, pooling
**Goal:** the game-feel layer.

- Skeletal clips from glTF + state machine + crossfades + notify events (`anim`).
- Camera rigs: first-person, third-person, gameplay↔cutscene blends.
- Built-in dialogue: typewriter + voice SFX + skip + advance-on-interact, themeable.
- In-game UI: anchors, drag-drop layout, scripted interactions (`floptle-ui`).
- Automatic object pooling API (take/return) (`floptle-core::pool`, ADR-0008).
- Advanced movement + **raycast-vehicle** controller on the SDF world, with
  baked sparse-field collision for the morphing fractal and `∂f/∂t` surface
  velocity so riders are carried by the shift (the "drive a car on a living
  fractal" goal).
- Deformable-matter **SoftBody** tier (XPBD particle cage) + simple **adhesion**
  (sticking/stretch) via `floptle-matter` — objects that squish, stick, and get
  carried; collide as SDF for free (ADR-0013).
- **Time-rate regions** (ADR-0017): a `TimeRegion` bullet-time dome slowing a
  region's morph/particles/physics while the rest of the world keeps moving.
- **Field-interaction seam + one proof** (ADR-0019): heat → a fractal wall melts
  where heated → density drops → surface gravity weakens and you slide off, shown
  with the rule-lens overlay. Three authored edges, nothing scripted.

**Demo:** a tiny vertical slice — walk up to an NPC, trigger dialogue + a cutscene
camera, swing an attack that spawns pooled "360Slash" VFX. And: drive a car
across a fractal that's morphing underneath you.

## Phase 9 — Scene management, export, packaging
**Goal:** ship a build.

- Scene transitions/loading; project settings; entry scene.
- `floptle-runtime` packs a project into a runnable build.
- Cross-compile + package for Linux, Windows, macOS.

**Demo:** export a standalone build of the Phase-8 slice and run it on all three.

## Phase 10 — Pre-release hardening & the "product" bar
**Goal:** good enough to be a Fopull LLC product.

- Replace **all** OoT temp assets with original Fopull art (ADR-0010).
- Default content: built-in textures, materials, shaders, a starter project.
- Docs/tutorials; choose final OSS license (ADR-0009); polish the theme.
- Build two reference games (one surreal adventure, one combat) to prove it.

**Demo:** the two reference games. If they were a joy to make, Floptle is ready.

## Later / opportunistic
- **Networking** (`floptle-net`): authoritative server build + clients on
  self-hosted infra (`subsystems/networking-future.md`).
- **Optional rigid-body dynamics** (`floptle-physics::dynamics`): object-vs-object
  stacking/joints/ragdolls — a lightweight custom impulse solver, or pull in
  `rapier3d`. Added only when a specific game needs it; the SDF world and the
  character/vehicle controllers don't depend on it.
- **Advanced deformable matter** (`floptle-matter`): the **Viscoelastic** tier —
  true MPM/MLS-MPM "soup", topological **fracture** (stretch sticky matter until
  it tears into stringy strands and splits), live re-meshing. Research-grade;
  sequenced after the core engine is solid (ADR-0013).
- **Calculated density-field gravity** (`floptle-physics::gravity` tier 3): derive
  `ρ(p)` from matter and solve Poisson `∇²Φ=4πGρ` (FFT/multigrid, Barnes-Hut) for
  emergent gravity in huge worlds (ADR-0014).
- **Hierarchical reference frames** (`floptle-core::frames`): galaxy→system→body→
  local frames so an entire galaxy is representable, not just a solar system
  (ADR-0015).
- **Research-grade rule systems** (layered on the thin seams once the look is
  proven): programmable light **Tier 3** (participating media, signed emission for
  "dark that emits"), the **full field-interaction graph** (cyclic, damped) +
  conserved transport, and **time reverse / echoes** (bounded entity traces)
  (ADR-0016 / 0017 / 0019).
- Job-system parallelism for sim/culling; deeper profiler; asset bundling.
- More raymarch primitives, more shader stdlib nodes, more post effects.
