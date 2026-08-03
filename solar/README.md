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

## Life, tools and the material economy

Planets grow their own plants and pay for their own rocks. The whole loop:

```
mine / harvest  →  suit pack  →  a craft's Cargo Bay  →  land at base
     →  Commerce Depot unloads + sells  →  money  →  research  →  better parts
```

**On foot**

| key | |
|---|---|
| `1` | **Mining Laser** — hold `E` to cut rock; what comes out depends on the world's archetype, how deep you are, and the ore vein you're standing in |
| `2` | **Harvest Cutter** — hold `E` to take down the plant you're facing |
| `3` | **Terrain Spade** — `E` lowers ground, `Q` raises it. Shaping only: it yields nothing |
| `I` | **Inventory** — the suit pack (45 kg, and mass is the only limit). Click a row to move that stack into a craft's hold or the warehouse when you're standing by one |
| `SPACE` / `CTRL` | swim up / dive, in water |

**The base buildings are walk-in.** The command centre, the assembly gantry and
the depot carry a *shell* collider generated from the model's own geometry
rather than one box around the whole thing, so their doorways are doorways —
walk in, stand under the roof, and the facility prompt still answers. The power
plant and the tracking dish are machines and stay solid. Regenerate a shell (and
check that a 2 m astronaut actually fits through the opening) with:

```
python3 solar/tools/shell_collider.py models/space-kit/hangar_largeA.glb --scale 4.2 --check
```

**The base is a place.** It is sited once — around the crew's landing site, after
the loading screen hands the world over — and then remembered body-relative in
the save (`base.body` + `base.x/y/z`), so reloading rebuilds the same colony on
the same ground however far you have wandered. It used to be re-cut around
wherever the astronaut stood when the terrain first answered, which on a loaded
save is the *loading hover* over the spawn planet's north pole: the buildings
went up there and the player woke up hundreds of metres away with nothing to walk
up to. If a prompt ever goes missing again, the Console says why — `facilities:`
reports how many are standing and how far the nearest one is.

**At the base** — the **Commerce Depot** (the fifth building) unloads any craft
parked within 140 m, then sells the warehouse. A material's **first** sale ever
is a discovery: a premium, a point of standing, and usually a part that just
became researchable. In the **builder**, a locked catalogue card shows what it
costs and what it wants; clicking it buys the research on the spot.

**What generates**

- `scripts/climate.lua` — temperature, moisture, elevation and biome anywhere on
  any world, from the record the generator publishes (`save.world.<name>`).
  Also owns the sea: where it is, and whether you're in it.
- `scripts/flora_gen.lua` — a world's species, rolled from its seed: silhouette,
  branch pattern, palette, glow, and what harvesting one gives you. The climate
  picks which forms a world can grow; the seed makes them unlike anywhere else's.
- `scripts/flora_field.lua` — streams them in around you, deterministically, so
  the same ground always grows the same stand. Cut plants regrow.
- `scripts/materials.lua` / `inventory.lua` / `research.lua` — the registry, the
  containers, and what's still locked.

**Tests** (offline, no engine needed):

```
luajit solar/tests/smoke_flora.lua        # climate, species, plant build, scatter, harvest
luajit solar/tests/smoke_resources.lua    # registry, containers, tools, depot, research
luajit solar/tests/smoke_company.lua      # ledger, contracts, deliveries
```

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
