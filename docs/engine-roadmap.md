# Engine Roadmap — from "great editor" to "ship a whole game"

Status: **Proposed** (2026-07-16)
Scope: gap analysis of the engine as it stands today + four workstreams to get Floptle to
"a team can build a complete, wide-genre game start-to-finish in this engine," including
the parry-MMO netcode push and the procedural solar-system demo.

This doc is grounded in a code audit (not memory) — file references below are current as of
this date.

---

## 1. Audit: where the engine actually stands

### Solid and shipped
- Scene/node model, prefabs, undo, multi-select, gizmos, docking editor
- Physics: rigidbodies (dynamic/kinematic/static), triggers, layers + collision matrix,
  SDF + mesh colliders, gravity volumes (uniform + **radial planet gravity** with
  auto-orient, `floptle-physics/src/gravity.rs`), floating origin (f64 anchors, exact to 1e7)
- Lua scripting: full lifecycle incl. `fixedUpdate`, collision/trigger hooks, params,
  vec math, prefab spawn/destroy, cross-node refs, tags, **masked raycasts**
  (`raycast(..., { layers = {"Ground","Props"} })` already works — `host.rs:403-436`)
- Runtime game UI: real GPU UI pass (shapes/images/**text**), Pin/Stack layout,
  `UiElement`/`UiSlider`/`UiLayer` script handles, button hooks — HUDs and menus work today
- Audio (spatial + mixer + effects + Lua), particles (timeline, curves, mesh particles),
  skeletal animation (CPU skinning, layered controller Lua API), shader IR + graph editor,
  post/shadows/AO, retro mode, vertex paint
- **Netcode is much further along than "basics":** QUIC (quinn) transport, relay server with
  lobby codes, changed-only snapshots + keyframes, `Replicated`/NetId, synced vars,
  prediction + reconciliation, input-clock auto-lead, **lag compensation with `net.rewind`**
  (poses AND synced vars, 250 ms clamp), stamped RPCs, per-owner spawn, scene-switch epochs,
  in-editor loopback harness with latency/loss sliders. ~4.2k LoC in `floptle-net` + 1.7k in
  the editor driver, unit-tested.
- Export: File ▸ Export Game works (editor binary in player mode, Win/mac cross-compile)

### Gaps (the subject of this roadmap)

| Area | Status | Notes |
|---|---|---|
| Render-layer masks (camera cull / light masks) | **Absent** | Layers exist only for physics/raycast. Cameras render everything. |
| Render-to-texture for gameplay (camera → UI image, minimap, mirrors) | **Absent as a feature, 90% built as infra** | `render_world_into` + offscreen targets already render any camera to a texture (`render_frame.rs`, `viewports.rs:47`) — but results are egui-only `TextureId`s; UI images & materials key on asset-path strings, no handle type. |
| Ortho projection / near-far control on scene cameras | Absent | `Projection::Orthographic` exists in the render crate but is unreachable from `Matter::Camera`. |
| Split-screen / secondary gameplay views | Absent | Single-active-camera pipeline; primitives support it, nothing wires it. |
| Save-game / persistence / prefs API | **Absent** | No script-facing storage of any kind. |
| Gamepad + rebindable input actions | **Absent** | `floptle-input` is an 11-line doc stub; scripts read raw keycodes. |
| Lua timers / wait / scheduler | Absent | Raw `coroutine` lib only; everything is manual `dt` accounting. |
| Seeded RNG + noise for Lua | Absent | Notable: a rollback-netcode engine with no deterministic RNG helper. |
| Additive scenes / persistent-across-scenes nodes | Absent | `scene.load` is a hard swap. |
| Pathfinding / navmesh / steering | Absent | |
| Shape queries (sphere/capsule/box cast, overlaps) | **Absent** | Only raycast. Trigger *events* exist but aren't on-demand queries and aren't lag-comp-rewound. Design doc §7 promises rewound overlaps — unimplemented half. |
| Slim runtime player | Skeleton | Exports ship the whole editor binary; `floptle-runtime` renders a demo scene, can't load a project, has zero net code. |
| Headless dedicated server | Designed-only | The reserved slot in netcode-design §9. |
| Interest management + delta compression | Designed-only | The two self-declared 2e leftovers — these ARE the MMO-scale features. |
| Chat / replicated VFX / replicated audio primitives | Absent | Convention via RPC only. |
| GPU skinning | Pending | CPU path works; rig data captured for GPU. |
| Animation events (keyframe → Lua callback) | Absent | Polling `finished()` only. |
| Runtime particle param control from Lua | Partial | play/stop only; no burst-count/track retune. |
| Terrain 2.0 | **Mid-migration** | New unbounded `ChunkField` + surface-nets mesher already RENDER in the editor, but **physics still collides against the old dense field with the 384-cell/576-unit cap**. P3 (chunk undo), P4 (LOD), P5 (streaming/clipmap), P6 (**runtime Lua terrain API** + splat shader) not done. |
| Procedural terrain generation | Absent | No noise/heightmap fill anywhere; hand-sculpt only. |
| Nested reference frames | Absent | `floptle-core/src/lib.rs` advertises a `frames` module that does not exist. Single 4096-unit rebase only. |
| Inverse-square gravity / orbital mechanics | Absent | `Point` gravity is constant-magnitude; f32 semi-implicit Euler; no orbit code. |
| Time-warp / time-scale | Designed-only | decision-0017 rate-field design; today one global clock. |
| Per-planet atmosphere / scattering | Absent | Sky stage is direction+time only (per-scene skybox). |
| Water physics (buoyancy/swim volumes) | Absent | water.flsl is visual-only. |
| In-engine image editor / texture painting | Partial seed | Mesh texture paint v1 exists but is in-session only; no 2D image editor. |

---

## 2. Workstream A — "Ship a game" fundamentals

Ordered by leverage. Most are small-to-medium; together they remove the "wait, the engine
can't do X?" moments that kill full productions.

### A1. Render targets + camera features (unlocks: minimaps, scopes, mirrors, security cams, trajectory map, split-screen)
- New **`RenderTexture`** handle: named offscreen target with size/format, allocated
  outside egui. Reuse `render_world_into` verbatim.
- `Matter::Camera` grows: `projection: Perspective|Orthographic`, `near`, `far`,
  `target: Screen | RenderTexture(name)`, `cullMask: Vec<String>` (reuse the existing
  `Layers::mask_of` name→bit table — do NOT invent a parallel layer system).
- `ImageSpec.texture` (floptle-ui) and the Material texture slot accept
  `rt://<name>` alongside asset paths → **camera-on-a-UI-image** just works.
- Per-light `layerMask` on PointLight/directional Light.
- Lua: `getcomponent("Camera")` grows `projection/near/far/target/cullMask`; cameras with
  render-texture targets render every frame while enabled.
- Split-screen = N enabled screen-cameras with viewport rects (stretch goal; the render
  loop just calls `render_world_into` per camera).

### A2. Persistence API (unlocks: literally any game with progress)
- `save.set(key, value)` / `save.get(key, default)` / `save.delete` / `save.flush` —
  NetValue-style guardrailed values (same marshalling as synced vars), JSON-on-disk per
  project + per save-slot (`save.slot(name)`).
- Server-side variant for multiplayer (server owns the file; clients request via RPC) —
  document the pattern, don't over-build.

### A3. Input system (unlocks: gamepads, rebinding, couch co-op)
- Implement the `floptle-input` stub: gilrs for gamepads + the existing winit keyboard/mouse.
- **Named actions**: project-level action map (`project.ron`) binding keys/buttons/axes →
  `input.action("jump")`, `input.axisAction("move")` (returns vec2). Old raw-key API stays.
- Editor UI: Input tab for the action map; runtime rebinding API (`input.rebind`).
- Actions ride the same input command struct → networked prediction gets gamepad for free.

### A4. Lua scheduler + tween (unlocks: readable gameplay code)
- Tick-based (deterministic, netcode-safe): `wait(seconds)` inside coroutines,
  `after(seconds, fn)`, `every(seconds, fn)`, driven from `fixedUpdate` ticks.
- `tween(node, {x=10, duration=0.5, ease="outQuad", onComplete=fn})` for transforms,
  UI element props, material params.

### A5. Seeded RNG + noise (unlocks: procgen demo AND deterministic netcode)
- `rng(seed)` object: `:next()`, `:range(a,b)`, `:pick(list)` — xorshift, serializable state.
- `math.noise(x,y,z)` / `math.fbm(x,y,z,octaves)` — same simplex the terrain generator
  (D2 below) uses, so Lua and Rust agree.

### A6. Scene management v2
- `scene.load(name, { additive = true })`, `scene.unload(name)`.
- `node.persistent = true` (survives scene swaps — the DontDestroyOnLoad equivalent).
- Async load + `scene.onLoaded` callback (loading screens).

### A7. Slim runtime + headless server (shared with Workstream B)
- Make `floptle-runtime` actually load a project (it has all crate deps already):
  `floptle-runtime --play <dir>`, `--server <dir> --port N` (headless, no window/GPU),
  `--connect <addr>`.
- Export ships the slim runtime + packed assets instead of the editor binary.

### A8. Animation/particles polish (as-needed tier)
- Animation events: named markers on clips → `animator():on("footstep", fn)`.
- GPU skinning (data is already captured; render path pending).
- Particles: `particles():burst(n)`, `particles():set("track.rate", v)` runtime overrides.

### A9. Pathfinding (defer until a game needs it)
- When it comes: SDF-aware — we have distance fields everywhere; flow-field or navmesh
  baked from walkable terrain mesh (Terrain 2.0 makes this natural). Not on the critical
  path for the parry MMO or the space demo.

---

## 3. Workstream B — Netcode: parry-MMO scale

Foundation (2a–2d) is real and tested. This workstream is the remaining 2e items + the
combat/product layer + scale proof. Order matters: test harness first, because everything
after it is only trustworthy if we can actually run many clients.

### B1. Multi-client test harness (the Roblox "Start with 3 players" button)
- Editor toolbar: **Play ▸ Networked ▸ [N players]**. Editor becomes/hosts the server
  (loopback or QUIC), then spawns N OS windows.
- v1 (this week-level): spawn N `floptle-editor --play <project> --connect <addr>` processes
  (player mode already exists; add `--connect`). Cascade window positions, label titles
  "Client 1/2/3", one-click Stop kills all.
- v2 (after A7): spawn slim `floptle-runtime` clients instead — faster boot, honest client
  behavior (no editor code paths).
- Also: `--bot <script.lua>` headless clients for load tests (see B4).

### B2. Combat toolkit (the Deepwoken parry layer)
- **Shape queries** in floptle-physics + Lua, all accepting the same `{layers=...}` options
  table as `raycast`:
  - `spherecast(origin, dir, radius, max, opts)` (swept)
  - `overlapSphere(center, radius, opts)` / `overlapBox(center, half, rot, opts)` →
    list of hits with `hit.node`
  - `capsulecast` for player-shaped sweeps
- **Lag-comp completeness**: overlap/shape queries inside `net.rewind` see the rewound
  world (design §7 promised this; only raycast honors it today).
- **Hitbox authoring**: a `Hitbox` component (shape + local offset + active window) +
  `node:hitbox():sweep(fromTick)` returning victims — so a melee swing is: play anim,
  activate hitbox for frames 8–14, engine returns rewound overlaps. Windup/active/recovery
  as data, not per-game math.
- **Parry certification harness**: an automated scenario (two bots, scripted attack/parry
  timings, latency/loss matrix 0–200 ms) asserting perceived-timeline correctness — the
  "never feels like you missed something you shouldn't" property as a regression test.

### B3. Replicated gameplay services (product layer, all thin sugar over RPC)
- `net.effect(key, pos, opts)` — replicated spawnEffect: fires locally + broadcasts to
  relevant peers (interest-managed once B4 lands), with client-side dedup for the caster.
- `net.sound(key, pos)` — same for one-shot spatial audio.
- **Chat primitive**: `net.chat.send(text, channel)`, `net.chat.on(fn)`, server-side
  rate-limit + filter hook, plus a default UI layer template (floptle-ui) games can restyle.
  Ships as the first "engine template" (prefab + script + UI layer).
- Team/permission helpers: `net.peers()` entries grow `peer.team`, server-set.

### B4. Scale: the actual MMO work
- **Interest management** (the design's grid + priority accumulator + per-client byte
  budget). This is THE feature between "8-player co-op" and "MMO server." Distance +
  recency priority, always-relevant sets (your own avatar, party), enter/leave =
  spawn/despawn on the client.
- **Delta compression** (baseline-delta against last-acked snapshot, per design §5;
  today is changed-only full values + keyframes).
- **Headless dedicated server** (A7's `--server`) + server tick budget instrumentation.
- **Load-test bots**: `floptle-runtime --bot bot.lua --connect …` running headless input
  scripts; a `scripts/loadtest` runner that ramps 10 → 50 → 200 → 500 bots against a server
  and records tick time, bandwidth/client, snapshot age. **Numbers before promises** —
  this tells us the real player ceiling and where it breaks.
- Bandwidth profiler UI (per-entity bytes, per-system bytes) in the net-stats overlay.

### B5. Ops (later, feeds Floptle Cloud)
- Server browser / lobby list on the relay, reconnect-with-token, spectator role,
  server-side Lua sandboxing budget (per-tick script watchdog).

---

## 4. Workstream C — In-engine image editor / texture painter ("🎨 Paint" tab)

The artist-expression bet. Seeds that exist: mesh texture paint v1 (in-session), vertex
paint, the shader graph, egui infra for canvas tools (wheel zoom / middle pan / box select
conventions already established).

### C1. Pixel editor core
- New dock tab editing project PNGs: layers (normal/multiply/screen…), pencil/eraser/
  fill/line/rect/ellipse, color picker + project palette swatches, marquee+move,
  mirror-X symmetry, grid + zoom-to-pixel, undo stack.
- **Hot-reload into materials on save** (mtime pipeline already exists for shaders) —
  paint → alt-tab-free live update on the mesh in the scene. This loop is the killer
  feature and mostly free given existing asset watching.
- New-image dialog (size presets, transparent/solid), import/export PNG.

### C2. Painterly/HD mode
- Brush engine: size/hardness/opacity/flow, spacing, tablet pressure (winit supports it),
  smudge, alpha-locked painting, blend modes per stroke.
- Layer masks + clipping layers. HSV/levels adjustments as non-destructive layer ops
  (GPU-evaluated — we have the shader stdlib; an adjustment is a tiny .flsl).

### C3. Paint-on-mesh v2 (persistence + pro tools)
- Make viewport texture painting write to real project textures (bake through UVs,
  seam-aware dilation), not just the in-session override.
- Project the 2D editor's brush engine into the 3D viewport (same brush code, different
  projection) — one brush system, two canvases.
- Texel-density warnings, UV overlay view in the Paint tab.

### C4. Procedural layers (the "only Floptle has this" feature)
- A layer whose content is a **shader-graph** (the ◈ editor) evaluated to pixels —
  parametric noise/patterns/gradients as live layers, re-editable forever, rasterized on
  export. Artists get Substance-like procedural texturing inside the same graph tool they
  already know.

---

## 5. Workstream D — The solar-system demo ("Floptle Solar")

The demo is a forcing function: every stage below ships a reusable engine feature first,
demo content second. Honest feasibility read from the audit: the *ground game* (land, walk,
dig, build on a planet) is near — it's mostly finishing Terrain 2.0. The *space game*
(system-scale distances, orbits, time-warp, trajectory map) is real foundation work.

### D0. Prerequisite: finish Terrain 2.0 (already signed off)
- **P3.5 — physics on `ChunkField`** (urgent even without the demo: rendering is meshed
  but physics still collides against the old capped dense field — a divergence bug farm).
- P3 chunk undo, P4 LOD rings, P5 streaming/clipmap, **P6 runtime Lua terrain API**
  (`terrain.sculpt/paint/height/query`) — P6 is the demo's dig/build mechanic verbatim.

### D1. Procedural planet generation
- Rust-side: `ChunkField::generate(gen: &PlanetGen)` — SDF composition: sphere ± fbm
  displacement (mountains), 3D ridged noise carve (caves), altitude/slope-driven palette
  bands, biome params. Deterministic from a seed. Generate lazily per chunk → planets are
  effectively free until visited (the sparse field was built for exactly this).
- Lua: `terrain.generate{ seed=…, radius=…, roughness=…, caves=…, palette=… }` +
  the A5 noise so scripts can scatter decoration nodes (rocks/trees/crystals) with the
  same seed.
- Planet "recipe" table (gravity strength, atmosphere params, palette, moon count) —
  one seed → a whole coherent planet vibe. This is the "every planet is different" knob.

### D2. Orbital mechanics — planets on rails (the KSP trick, not n-body)
- Do NOT build an f64 n-body integrator. KSP itself puts celestial bodies **on rails**
  (analytic Kepler ellipses, position = f(time)) and only integrates the ship. Adopt that:
  - `OrbitRail` component: Kepler elements around a parent body; position evaluated
    analytically from mission time — stable forever, trivially time-warpable.
  - Ship: add an **inverse-square** `GravitySource::PointInvSq { mu, … }` variant and
    integrate the ship in f64 against the dominant body (patched conics: one body at a
    time, switch on sphere-of-influence).
- **Reference frames**: implement the advertised-but-missing `frames` module in
  floptle-core — minimum viable version: every node optionally parents to a **frame**
  (a planet); physics + rendering run in the local frame of the dominant body; frame
  origin rides the rails in f64. This dodges system-scale f32 coordinates entirely and
  is the single biggest engineering item in the demo. (Scoped v1: non-rotating frames,
  one active frame per client, hand-off on SOI switch.)

### D3. Time-warp
- v1 pragmatic: on-rails warp — when the ship is out of atmosphere/physics range, switch
  it to a rail too and scrub mission time at 10×–10,000× (pure function evaluation, no
  physics). Drop back to live physics on warp-exit. This is exactly KSP's model and needs
  no rate-field machinery.
- The decision-0017 `LocalTime` rate field remains the long-term general feature; don't
  block the demo on it.

### D4. Trajectory map
- **Dogfoods A1**: an orthographic map camera rendering orbit-line gizmos to a
  RenderTexture shown on a UI image (or fullscreen map mode). Conic sections are analytic
  (rails + patched conics) → drawing the predicted path is evaluating the ellipse, not
  simulating. Maneuver nodes v2.
- Uses A4 (tweens for map zoom), A6 (UI screens), B-chat optional.

### D5. Atmosphere + water
- Extend Sky stage inputs: `cameraPos`, `altitude`, per-scene→per-planet param blocks —
  density-by-altitude tint, horizon haze, space-black above. A cheap analytic scattering
  approximation (not Bruneton) is plenty for the art style.
- Water volumes: a `WaterVolume` (region + surface height) driving buoyancy force +
  swim-mode character state + underwater fog/tint post toggle. Flooded-cave planets
  come from D1 carving caves below the water table.

### D6. Demo assembly
- Ship (rigidbody + thrusters via Lua + landing legs), enter/exit, on-foot explore,
  dig (P6), build (P6 + prefab placement), per-planet recipes, seed selector on the
  title screen (A2 saves the game). Every unique visual = the shader graph flexing.

---

## 6. Sequencing

Dependency-honest order, interleaving so each phase ships something visible:

1. **Terrain 2.0 completion (D0)** — signed off, mid-migration, physics/render divergence
   is a live risk. Includes P6 runtime terrain API.
2. **Core wave 1 (A1 render targets + masks, A2 save, A5 RNG/noise, A4 scheduler)** —
   four small-medium features, all prerequisites for later workstreams.
3. **Netcode B1 (multi-client harness) + B2 (combat toolkit)** — B1 first since B2's
   verification depends on it. Parry certification harness closes B2.
4. **A7 slim runtime + headless server** — upgrades B1 to v2, unblocks B4.
5. **B4 scale (interest mgmt, delta compression, load-test bots)** — run the ramp, get
   real player-count numbers.
6. **C1 pixel editor** (+ C2 as appetite allows) — parallelizable with 4–5; it's
   editor-side and touches nothing the netcode work touches.
7. **A3 input actions + gamepad**, A6 scenes v2, B3 services (chat/effects) — the
   "polish wave" before demo work.
8. **Demo: D1 → D2 → D3 → D4 → D5 → D6.** D2's frames module is the long pole — start
   its design doc during phase 5.

## 7. Open decisions for Ty

1. **Sequencing sign-off** — the order above front-loads netcode before the image editor;
   flip 3–5 and 6 if artist tooling feels more urgent.
2. **Player-count target** for B4's load ramp (100? 500?) — sets how hard interest
   management must work and whether server-side spatial partitioning across cores is in
   scope.
3. **Demo scope**: single star system, how many planets for v1 (suggest 4–6 recipes:
   flooded-caves, icy-lowgrav, thick-atmo-highgrav, barren-moon…)?
4. **Frames v1 scope**: non-rotating frames acceptable (planets don't spin) for the demo?
   Rotating frames are a large extra step.
5. **Image editor identity**: separate "🎨 Paint" dock tab agreed? Pixel-first (C1) then
   HD (C2), or HD-first?
