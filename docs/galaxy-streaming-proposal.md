# Galaxy-Scale Streaming — terrain residency, on-demand generation, multi-system worlds

Status: **G1 implemented** (terrain residency streaming) · G2–G5 proposed
Author: Fable, 2026-07-19 · requested by Ty ("simulate massive galaxies… travel across an
entire galaxy without the computer dying")

## The goal

Fly from a planet's cave system to orbit, to another planet, to another *star system* —
seamlessly, with the machine only ever paying for what's near the camera. Ty's framing is
exactly right: **distance-priority tradeoffs at runtime** — full terrain detail near you,
simple shaded spheres far away, and (eventually) whole star systems as dots until you
approach.

## What already exists (foundations we build on)

The engine is closer to this than it looks — the missing piece is *CPU-side residency*:

| Layer | Status |
|---|---|
| f64 world coordinates (ADR-0015) | ✅ sub-mm at 12 light-years, camera-relative GPU upload |
| Floating-origin physics | ✅ 4096-unit rebase, snap-free |
| Analytic on-rails orbits | ✅ per-tick Kepler eval, drift-free at any warp, cost ~ns/body |
| Terrain 2.0 sparse fields | ✅ `ChunkField` authority, chunk meshes, LOD rings, skirts |
| Far-body impostors (GPU) | ✅ beyond 60 body-radii: streaming stops, chunk meshes + SDF atlas slots evicted, one shaded sphere drawn |
| Patched-conic gravity | ✅ only the dominant body pulls — O(bodies) per query |
| 4-star lighting | ✅ brightest-at-camera selection already picks the near system |

**The blocker**: every terrain node's `ChunkField` (15–100 MB) *plus* its dense shadow
proxy (~10–50 MB) loads into RAM at scene open and stays forever; Play snapshots double
it. 25 bodies ≈ several GB. That's what dies first — not the GPU, not the orbits.

## G1 — Terrain residency streaming (IMPLEMENTED)

Three states per celestial terrain, driven by camera distance in body radii:

```
        0 ──────── 60 r ────────── 80 r ───────── 110 r ──────→ ∞
        [ meshed terrain ][ impostor sphere ][ impostor sphere  ]
        [   field in RAM  ][  field in RAM  ][   field COLD     ]
                            ← load at 80 r      evict at 110 r →
```

- **Cold** = no field in RAM at all. The body still orbits on rails, still draws as its
  impostor sphere (color comes from a tiny `.meta` sidecar written next to the
  `.cfield`), still shows on the map. It just costs ~nothing.
- **Load at 80 r, evict at 110 r** — both thresholds sit *outside* the 60 r impostor
  flip, so residency changes are **visually invisible**: you only ever watch an impostor
  become terrain through the already-shipped chunk streaming, never a pop.
- Loads run on a background thread (mpsc, same pattern as the planet generator); evicts
  in edit mode **save the field first** (a dug cave 110 radii away is never lost), evicts
  during Play just drop (Stop reverts terrain anyway — same semantics as today).
- Terrains loaded during Play are dropped back to cold on Stop (Play must not leak
  residency), and a mid-Play arrival rebuilds the sim (`rebuild_sim`) so the new
  planet's collider exists — the established mid-Play rebuild path.
- **Play start force-loads** any cold body near a dynamic body or the camera,
  synchronously — the spawn planet always has collision before the first tick.
- **Warp guard**: time-warp travel crosses 80 r in milliseconds — the warp coast drops
  to 1× when the ship comes inside 60 r of a *cold* body (Console explains), giving the
  stream seconds to land; an emergency synchronous load fires at 5 r as a last resort
  (teleports, `L` summons). Re-engage warp once it's resident.
- Non-celestial terrains (flat game levels) are always resident — residency is a
  planets feature.
- `.meta` sidecar self-heals: a scene from before G1 loads each field once, computes
  the impostor color, writes the sidecar, and can go cold from then on.

**What this buys today**: a 25-body system costs RAM for ~2–4 bodies (the planet you're
at, its moons, maybe a neighbor) instead of 25. The generator's planet cap rises
accordingly. Scene open gets *faster* (near-body loads happen in the background while
the editor is already interactive).

## G2 — On-demand deterministic generation (the galaxy enabler)

For thousands of bodies, even disk files are wrong: a galaxy's terrain should be
**generated when first approached, from its seed** — No Man's Sky's core trick, and we
already have the machinery (`terrain.generatePlanet` runs `PlanetFill` on a thread;
fills are deterministic per seed).

- Terrain nodes gain an optional **genspec** (the serialized `PlanetFill`) — written by
  the system generator instead of pre-generating every field.
- Residency's load step becomes: cfield file exists → load it; else genspec present →
  generate in the background (~4–10 s, hidden by the 80 r lead distance + warp guard).
- A body you never visit costs **one scene node** (~1 KB). A body you dug on saves its
  cfield on evict and loads the dug version forever after — visited worlds accumulate
  history, untouched worlds stay procedural.
- Impostor color without a field: the generator computes it from the genspec's surface
  palette at roll time and stores it on the node — no terrain needed.
- This also fixes the repo/disk problem at its root: only *visited* worlds have big
  files.

## G3 — Multi-system galaxies

- **Hierarchical rails already work**: parent-by-name chains (moon→planet→star) — a
  galaxy is stars parented to a galactic barycenter (or fixed positions; real stellar
  orbits are imperceptible). Rails cost is linear and trivial per tick.
- **A star system is a group node** (the G1' clean-hierarchy format, shipped): the
  generator gains a galaxy mode — N systems, each a `"<Star> System"` group at a
  galactic position, bodies inside. Cleanup/regeneration semantics carry over
  unchanged.
- **Far systems render as their star**: the star's emissive sphere with
  distance-floored radius (`r_eff = max(r, dist·0.0022)`, already shipped for
  impostors) reads as a bright dot from light-years out — the skybox starfield gains a
  few *real* stars you can actually fly to. Planets of far systems skip rendering
  entirely (angular size gate) — they're sub-pixel behind their star anyway.
- **Lighting**: the 4-star uniform limit + brightest-at-camera selection already does
  the right thing — you're lit by your local sun; distant suns contribute dots, not
  light.
- **Physics**: patched conics make remote systems free — nothing simulates there. The
  SOI walk is O(bodies); at 10⁴ bodies introduce a per-system bounding test first
  (two-level walk: which system, then which body).
- **Travel**: time-warp at 10⁵× covers interstellar distances; G3 adds a per-distance
  warp ladder cap (interplanetary 10⁴, interstellar 10⁶) — tuning, not architecture.
- **Precision**: f64 holds sub-mm to ~12 ly; beyond that, either accept mm-scale (fine
  — nothing is small out there) or add a per-system local origin (rails already
  compute parent-relative; only the render anchor needs the system origin). Defer
  until a galaxy actually exceeds ~10 ly.

## G4 — Polish the seams

- Live collider add (`Sim::add_collider`) instead of mid-Play `rebuild_sim` on arrival.
- Shadow-proxy derivation moves to the background load thread (it's the expensive half
  of `EditorTerrain::new`).
- Byte-budget manager: hard RAM cap, evict lowest angular size first even inside 110 r
  (protects against pathological many-moon closeups).
- Predictive loading along the ship's *conic* (not camera position) — start the stream
  where you'll BE, not where you are; kills the warp guard's 1× drop for planned
  encounters.
- Impostor → terrain cross-fade over ~0.5 s (today the chunk streaming pops in over a
  few frames; a fade would hide even that).

## G5 — The experience layer

- Galaxy map mode (the existing 3D map, one zoom level up: systems as nodes).
- Interstellar maneuver planning (the across-SOI node planner generalizes — a star is
  just another SOI).
- Discovery/naming UI, per-system save metadata, "first visited" timestamps — the
  exploration-game loop on top.

## Decision points for Ty

1. **G2 genspec format**: store the `PlanetFill` on the terrain node (scene-file RON,
   my preference — survives regeneration, diffable) vs. a sidecar registry file?
2. **Galaxy scale target**: dozens of systems (all-rails, f64, one scene — simple) or
   thousands+ (needs the two-level SOI walk + system-local origins)? Shapes G3.
3. **Warp guard feel**: is the drop-to-1× at a cold body's 60 r acceptable, or should
   G4's predictive conic loading come sooner?

## Order of work

G1 (done) → generator caps up → **G2** (small engine surface: genspec field + generate-
on-load; biggest payoff per line of code) → G3 galaxy generator + far-system rendering
→ G4/G5 as the experience firms up.
