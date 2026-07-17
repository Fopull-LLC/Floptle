# Floptle Solar

The engine's space-demo project (see `docs/engine-roadmap.md`, Workstream D): the
long-term goal is a procedurally generated solar system — fly a ship between seeded
planets, land, explore, dig, build. This is its first slice.

## What's here now

- **`scenes/planetoid.ron`** — a little generated planet at the origin with **radial
  gravity** (walk all the way around it), a third-person astronaut, and a dig tool.
- **`terrain/planetoid.1.cfield`** — the planet itself: a seeded, noise-displaced
  sphere in the sparse Terrain 2.0 chunk field, with a cave network hidden under the
  surface. **Dig to find it.**
- **`scripts/dig_tool.lua`** — hold **LMB** to dig where you aim, **Q** to pile
  ground back up (the runtime `terrain.*` API).

Open the project in the editor and press Play. Controls: WASD + Space (third_person),
RMB-drag to look (third_person_camera), LMB dig, Q build.

## Regenerating the planet

```
cargo run --release -p floptle-field --example gen_planetoid -- solar/terrain [seed]
```

Different seed = different planet (relief, colour patches, caves). Knobs via env:
`RELIEF=6 CAVES=0 ...`. A headless render of the current field:

```
cargo run --release -p floptle-render --example solar_probe
```

writes `solar_orbit.png` / `solar_surface.png`.

## Where this is going (roadmap D1–D6)

Planets on Kepler rails + inverse-square gravity + patched conics for the ship,
nested reference frames, on-rails time-warp, an orbital trajectory map rendered to a
UI viewport, per-planet atmospheres via the Sky stage, water volumes. Each stage
lands as an engine feature first — this project is the proving ground.
