# Floptle Solar — full demo plan (KSP-class)

*2026-07-17. Supersedes workstream D of `engine-roadmap.md` with a concrete build
order. Goal per Ty: a massive, genuinely impressive demo — a generated solar
system with realistic orbital mechanics, a pilotable ship you can board/exit,
KSP-style trajectory map, time-warp, landing/takeoff with gear, digging, and a
context-aware space sky with atmospheres.*

## The one architectural decision everything hangs on

**Scaled-realistic, on-rails system, f64 ship.** Real physics *formulas* at
KSP-like scale — not real solar-system distances:

- Planets: radius 150–600 units of walkable voxel terrain (10–20× today's
  planetoid). Moons smaller; the sun is a light + shader, not terrain.
- Orbits: 20k–2M units, computed **on rails** (Kepler elements → position at
  time *t*, f64). No n-body integration for planets — orbits are exact, stable
  at any warp, and the trajectory map can draw them analytically.
- The ship integrates in f64 under the **dominant body's** inverse-square
  gravity (patched conics / sphere-of-influence, exactly KSP's model). When
  coasting, the ship converts its state vector to orbital elements and rides
  rails too — that is what makes 100 000× warp trivial and drift-free.
- Planets do not rotate in v1 (non-rotating body frames). Revisit only if the
  demo needs day/night on the surface.
- Rendering stays f32 near the camera: the existing floating origin (4096-unit
  rebase) already covers this; body positions come from rails each frame.

## Build order (each stage is playable when it lands)

### S0 — Dig CSG fix ✅ *(shipped 29c7793)*
`Brush::Lower` was algebraically inverted (`max(cur, ball)` — keep the ball,
carve the box). Every dig blasted a write-box-sized square crater. Root cause
of "massive squares"; found by the new `dig_probe` render example, pinned by a
regression test.

### S1 — A4: Lua scheduler *(engine, small — unblocks all demo scripting)*
`after(s, fn)`, `every(s, fn) → handle:cancel()`, `tween(obj, key, to, s, ease)`.
Tick-driven and deterministic; timers advance ONLY in the global `run_fixed`
(never in `run_fixed_for`/replay paths, or netcode prediction double-fires
them). Demo needs it for: warp ramps, staging sequences, gear animation, HUD.

### S2 — Space core: `frames` + Kepler rails + patched conics *(engine crate work, the long pole)*
New `floptle-core::frames` (the module the lib docs already advertise):
- `Body { id, parent, mu, radius, soi, elements: Kepler }` registry; system
  time `t` (f64 seconds, owned by the sim tick, warp-scalable).
- `Kepler::pos_vel(t)` (elliptic + hyperbolic), `Kepler::from_state(r, v, mu)`
  — the two conversions everything else is built from. Property-tested
  round-trip + energy/angular-momentum conservation.
- Scene side: a `CelestialBody` component (or scene-level system table) maps
  nodes to bodies; each frame the engine writes body node positions from rails.
- Gravity: `PointInvSq` volume mode (µ/r², dominant-SOI selection) replacing
  the current constant radial `GravityVolume` for space; surface gameplay keeps
  working because near-planet µ/r² ≈ constant.
- Lua: `space.bodies()`, `space.soi(pos)`, `space.elements(node)`,
  `space.time()` — the map + HUD read these.

### S3 — Ship v1: board / fly / land *(mostly Lua in `solar/`, small engine assists)*
- Boarding: interact key near the hatch → astronaut node hides + controller
  swaps to the ship (existing handles/parenting; no engine change expected).
- Flight: ship = dynamic rigidbody; `ship_controller.lua` — main throttle
  along +Y of the ship, RCS torque (pitch/yaw/roll), SAS = PD damping toward
  a hold orientation. Needs Lua `rig.addForce/addTorque` in the BODY frame if
  not already expressible.
- Landing gear: child nodes + colliders toggled/animated via scheduler; simple
  ground-contact readout.
- Exit → walk → dig → re-board loop closes on the current planetoid.

### S4 — Time-warp *(engine hook + rails payoff)*
`space.warp(mult)`: 1×–4× = physics warp (more ticks); above that the ship
must be coasting → snap to rails, advance `t` analytically, reject warp under
thrust/atmosphere like KSP. Editor Play gets a warp readout in the HUD.

### S5 — A1: render targets + camera masks *(engine — the map + HUD enabler)*
RenderTexture handle; camera `cullMask` / ortho / near-far / `target`; `rt://`
bindings usable in UI images + materials; per-light masks via `Layers::mask_of`.
Also what camera-feed screens inside the ship cockpit want.

### S6 — Trajectory map *(the KSP map screen)* — **SHIPPED v1 (differently)**
Plan predated `stage ui` shaders; v1 landed as a pure UI-shader panel instead
of a line-draw layer + dedicated camera — zero engine changes:
- `solar/shaders/map.flsl` draws the whole map: the ship's conic in its own
  orbital plane via the trig-free focal-polar form `len(q) + e·q.x = p`
  (ellipse AND hyperbola), Pe/Ap markers, focus body + SOI, the sibling
  body + its orbit ring + its SOI ring (transfers planned by eyeballing your
  conic against the moon's SOI), ship marker + velocity tick, starfield.
- `ship_controller.lua` does the orbital mechanics (e-vector basis, plane
  projection) and feeds uniforms via `setShaderParam`. M toggles, ↑/↓ zoom,
  auto-fit on open; works during warp. Time-warp keys already global (./,).
- Still future (needs A1/line-layer): free 3D map camera, timed SOI-encounter
  marker from a patched-conic walk, maneuver nodes.

### S7 — Generated solar system + big planets *(procgen scale-up)*
- `gen_solar` example → `solar/system.ron` + N `.cfield`s from one master
  seed: 3–5 planets + 1–2 moons, per-body palette/relief/cave character/size,
  Kepler elements assigned for pleasing spacing.
- Big planets: radius 150–600 units. Sparse chunks scale with *surface area*
  — a 300-radius planet at voxel 1.0 is ~1–2 GB dense but ~100–300 MB sparse;
  validate + tune band/quantization (this is where Terrain P5's quantized
  band pays off if memory bites).
- Far bodies: **impostors** — terrain streams in/out by distance (the LOD
  worker already exists), beyond that a matcap/shaded sphere mesh + atmosphere
  halo, beyond that a bright dot the sky shader draws.

### S8 — Atmospheres + contextual sky *(the vibe stage)*
- Per-body `Atmosphere { color, density, height }` in the system definition.
- Sky stage gets engine inputs: camera altitude over dominant body, sun dir,
  that body's atmosphere params — deep space (stars, milky band) blends to
  horizon glow on approach to full scattering gradient + fog on the surface.
  One `space_sky.flsl` driven by those uniforms; no per-planet shader forks.
- Cheap orbit-visible touches: atmosphere rim on the planet impostor, sun
  bloom, star parallax.

### S9 — Assembly & polish *(game content)*
Ship interior/cockpit, HUD (altitude/velocity/apoapsis readouts — text overlay
v1), sounds (engine rumble via existing audio API), saves via `save.*`
(ship orbit + dug terrain deltas), README tour script, trailer route.

## Sequencing rationale
- A4 first: 30× smaller than anything else here and every later stage scripts
  with it.
- S2 before ship/map/warp: rails + conversions are the foundation all three
  read from. S3 ships against the *current* single planetoid so piloting feel
  iterates early (Ty playtests while S4–S6 land).
- A1 exactly before the map (S6) — first real consumer; also unblocks roadmap
  workstreams (UI images, minimaps) beyond the demo.
- Procgen scale-up (S7) after flight: planet size/spacing wants tuning against
  real travel/warp feel, not guesses.
- Atmosphere last-but-one: pure presentation, reads everything else's state.

## Open decisions for Ty (defaults chosen, flag if wrong)
1. **Scale**: KSP-style scaled system (default) vs literal real-scale — real
   scale means hours of warp and f64 through the whole render path; not
   recommended.
2. **Rotation**: planets don't spin in v1 (default).
3. **Map UI**: v1 full-screen mode toggle (default) vs composited UI panel —
   panel version wants the UI-system proposal decisions answered first.
4. **One ship** hand-built as a prefab (default) vs part-based construction —
   construction is a whole second game; recommend v2.
