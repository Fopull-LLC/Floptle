# Generating planets

`terrain.generatePlanet(id, opts)` fills a terrain with a whole world: a sphere of ground with relief, caves, craters and layered materials. The same `opts` table also works with `node:setTerrainGen(opts)`, which generates a planet the first time a player comes near it. This page covers the layers that make worlds differ from each other: mountain ranges, warped and terraced ground, overhangs, ravines, rivers, and surface materials picked by climate.

Every key is optional, and so is every key inside a layer's table. A planet with none of the layers on this page is the same planet it always was, voxel for voxel.

## The relief layers

| Key | What it makes | Keys inside |
| --- | --- | --- |
| `mountains` | Ranges of ridged peaks where a slow mask is high | `height` (crest height in metres), `scale` (ridges across the sphere), `sharpness` (higher is thinner, steeper crests), `coverage` (0 to 1, how much of the world has ranges), `octaves` |
| `warp` | Bends the relief into folded, wind-carved shapes | `strength` (how far it pushes, as a fraction of the radius), `scale` |
| `plateau` | Rounds off every peak above a height into a table top | `height` (metres above the radius), `smooth` |
| `terrace` | Steps the ground into terraces, or mesas with `plateau` | `step` (metres), `sharpness` (0 is a gentle slope, 1 is a sheer rise) |
| `overhangs` | Cliffs that lean out over their own foot, and arches | `depth` (how far from the surface the effect reaches), `scale` (per metre), `strength` (metres) |
| `ravines` | Steep-walled cuts with flat floors | `count`, `depth`, `width`, `length`, `meander` |
| `rivers` | Rivers that run downhill from the highest ground, each in a valley it carves | `count`, `depth`, `width`, `seaLevel` (where they end, metres above the radius), `meander` |

Mountains, warp, plateaus, terraces, ravines and rivers all shape the ground's height in each direction. `overhangs` is the one that works in three dimensions, so it is the one that can make ground with air under it.

Rivers start at the highest ground and follow the slope down until they reach `seaLevel` or a basin they cannot leave. A river that ends in a basin is where a lake belongs. To find where the rivers and ravines went without generating anything, call `terrain.planetRivers(opts)` with the same opts:

```lua
for _, ch in ipairs(terrain.planetRivers(opts)) do
  if ch.river then
    local p = ch.points            -- x1, y1, z1, x2, y2, z2, ... planet-local
    print("a river of " .. #p // 3 .. " points, " .. ch.widths[1] .. " m wide at its source")
  end
end
```

The points lie on the sphere of the planet's radius; the carved bed is below the ground there.

## Lakes

`terrain.planetLakes(opts)` answers where water would settle on the same planet, again without generating it. Every basin is filled to the height it spills at, and each connected stretch of water is one lake:

```lua
for _, lake in ipairs(terrain.planetLakes(opts, { minDepth = 1 })) do
  print(string.format("a lake at %.1f m, %.0f m across, %.1f m deep", lake.level, lake.reach * 2, lake.depth))
  -- lake.center is planet-local, on the lake's surface: where to put its water.
end
```

Ground below the rivers' `seaLevel` is sea, not a lake; pass `seaLevel` to set it yourself. With no sea at all, the planet's lowest point is where it drains, and it holds no lake. Lakes are found on a grid of cells about `cell` metres across (3 by default), and only water deeper than `minDepth` (0.5 m by default) counts.

## Surface materials by climate

`biomes` is a list of rules. For every patch of surface, the first rule whose ranges all match paints it with that rule's `slot` and `color`. A range left out does not constrain. Ground that no rule matches falls back to `surfaceA` and `surfaceB`, as before. Deeper layers (`subsoil`, `strata`, `deep`, pockets and seams) are unchanged.

| Key | Range of |
| --- | --- |
| `latitude` | Degrees from the equator, north or south: `{70, 90}` is the poles |
| `altitude` | Metres above the radius: `{-2, 1}` with rivers' `seaLevel` 0 is a beach band |
| `slope` | Degrees from level: `{40, 90}` is cliffs |
| `moisture` | A moisture pattern from -1 (dry) to 1 (wet); `moistureScale` sets how large its patches are |
| `nearRiver` | Within this many metres of a river (a single number) |

Order matters, because the first match wins. In the alpine example below, the cliff rule comes before the snow line, so cliff faces stay bare rock even above the snow line.

## Example worlds

Each of these is a complete `opts` table, at a size that generates in a second or two. Change `seed` for another world of the same kind.

**Alpine.** Sharp ranges over rolling lowland, snow above 30 m, bare rock on the steep faces, rivers down the valleys.

```lua
terrain.generatePlanet(1, {
  seed = 4, radius = 260, voxel = 1.5, relief = 10, caveDepth = 40,
  mountains = { height = 55, scale = 3.2, sharpness = 2.5, coverage = 0.45 },
  rivers = { count = 8, width = 8, depth = 4, seaLevel = -4 },
  biomes = {
    { slot = 3, color = { 0.55, 0.55, 0.58 }, slope = { 38, 90 } },
    { slot = 12, color = { 0.95, 0.96, 1.0 }, altitude = { 30, 999 } },
    { slot = 1, color = { 0.35, 0.55, 0.3 }, nearRiver = 20 },
  },
})
```

**Canyon desert.** Warped, terraced red rock cut by deep ravines.

```lua
terrain.generatePlanet(1, {
  seed = 9, radius = 260, voxel = 1.5, relief = 14, caveDepth = 30,
  warp = { strength = 0.18, scale = 2.5 },
  terrace = { step = 5, sharpness = 0.85 },
  ravines = { count = 6, depth = 28, width = 18, length = 600, meander = 1.2 },
  surfaceA = { slot = 2, color = { 0.78, 0.45, 0.28 } },
  surfaceB = { slot = 2, color = { 0.86, 0.6, 0.38 } },
})
```

**Archipelago.** Ground that falls below a sea level, so the high points stand as islands with sand round them, grey seabed below and green where it is wet.

```lua
terrain.generatePlanet(1, {
  seed = 21, radius = 260, voxel = 1.5, relief = 18, caveDepth = 20,
  mountains = { height = 22, scale = 4.5, coverage = 0.5, sharpness = 1.6 },
  rivers = { count = 5, seaLevel = 2 },
  biomes = {
    { slot = 3, color = { 0.42, 0.45, 0.5 }, altitude = { -999, -1 } },
    { slot = 4, color = { 0.92, 0.86, 0.66 }, altitude = { -1, 2.5 } },
    { slot = 1, color = { 0.3, 0.6, 0.32 }, moisture = { -0.2, 1 } },
  },
})
```

The sea itself is not part of the terrain: place a WaterVolume at the sea level.

**Karst arches.** Steep overhanging limestone towers on a flat plain.

```lua
terrain.generatePlanet(1, {
  seed = 30, radius = 220, voxel = 1.5, relief = 4, caveDepth = 50,
  mountains = { height = 48, scale = 9, sharpness = 5, coverage = 0.65 },
  plateau = { height = 34, smooth = 1.5 },
  overhangs = { depth = 16, scale = 0.07, strength = 7 },
  biomes = { { slot = 3, color = { 0.82, 0.8, 0.74 }, slope = { 30, 90 } } },
})
```

**Salt flats.** Flat white pans in the basins, low mesas between them.

```lua
terrain.generatePlanet(1, {
  seed = 55, radius = 260, voxel = 1.5, relief = 9, caveDepth = 25,
  plateau = { height = 6, smooth = 4 },
  terrace = { step = 3, sharpness = 0.6 },
  biomes = {
    { slot = 5, color = { 0.96, 0.95, 0.92 }, altitude = { -999, -2 } },
    { slot = 2, color = { 0.7, 0.6, 0.5 }, slope = { 20, 90 } },
  },
})
```

## What it costs

A planet with any of these layers works its ground height out once per direction, on a grid about a voxel across, before it fills a voxel. Each voxel then reads the grid instead of evaluating the layers, so they cost little: a 400 m world with mountains, warp, terraces, overhangs, ravines and rivers generates in about a fifth longer than the same world without them, most of that because mountains give it more surface to fill. Rivers are traced on a coarser copy of the same ground, which is why `terrain.planetRivers` answers quickly.

### Leaving the caves for later

From outside, a planet is its surface. Its caves are sealed under at least three voxels of crust, so nothing sees them until a dig breaks in. `deferCaves = true` takes advantage of that: the generator fills the surface and keeps every chunk that lies wholly in the buried cave zone as plain rock, then makes the real chunk the moment a dig, or any edit, reaches it.

On a 400 m world with an 80 m cave zone that leaves about a quarter of the chunks for later, and the planet fills and keeps about a quarter less. A chunk comes out the same whichever order chunks are made in, so two players' games agree on its caves. It is evened out on its own rather than with its neighbours, so where it meets a chunk the first fill made, its walls can sit a hair (well under a voxel) from where filling the whole planet at once would put them.

A saved field remembers which chunks are still to make; the body's generation spec makes them when it is loaded again.
