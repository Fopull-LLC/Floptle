-- CLIMATE — what a place on a planet is LIKE, and therefore what grows there.
--
-- The generator rolls a world's physical facts once (archetype, radius, relief,
-- how much sunlight it gets, whether it has air, where its sea sits) and
-- publishes them to `save.world.<name>`. This script turns those facts plus a
-- position into the three numbers everything downstream actually wants:
--
--     temperature   0 = frozen solid, 1 = scorching
--     moisture      0 = bone dry,     1 = swamp
--     elevation     metres above (or below) the sea, or above the mean surface
--                   on a world that has no sea
--
-- …and the BIOME they add up to. Flora density and species mix come from the
-- biome; so do the shoreline, the snow line and where an ocean starts.
--
-- The model is deliberately simple and deterministic: latitude cools you, height
-- cools you, the sea wets you, and a seeded noise field does the rest. No
-- weather simulation — the point is that two planets rolled from different seeds
-- feel like different WORLDS, not that any one of them is meteorologically real.
--
--     local cl = findScript("climate")
--     local s = cl.sampleAt("Golil", x, y, z)   -- {biome, temp, moist, elev, …}
--     cl.isUnderwater(x, y, z)                  -- for swimming / splashdowns

defaults = {}

-- Biome table. `flora` scales scatter density (0 = nothing grows), `wet` marks
-- the underwater ones, and `label` is what a survey readout says.
BIOMES = {
  ocean    = { label = "Ocean",         flora = 0.55, wet = true },
  reef     = { label = "Shallows",      flora = 1.00, wet = true },
  shore    = { label = "Shoreline",     flora = 0.85 },
  wetland  = { label = "Wetland",       flora = 1.30 },
  forest   = { label = "Forest",        flora = 1.45 },
  jungle   = { label = "Jungle",        flora = 1.80 },
  grass    = { label = "Grassland",     flora = 0.95 },
  steppe   = { label = "Steppe",        flora = 0.45 },
  desert   = { label = "Desert",        flora = 0.18 },
  tundra   = { label = "Tundra",        flora = 0.30 },
  snow     = { label = "Snowfield",     flora = 0.08 },
  highland = { label = "Highlands",     flora = 0.35 },
  ash      = { label = "Ash Waste",     flora = 0.10 },
  crystal  = { label = "Crystal Flats", flora = 0.40 },
  barren   = { label = "Barren",        flora = 0.0 },
}

-- Archetype character: base temperature, base moisture, and whether the world
-- can hold liquid water at all. These are the planet's personality before
-- latitude and altitude get a say.
local KIND = {
  canyon  = { temp = 0.55, moist = 0.45, seas = true },
  dune    = { temp = 0.78, moist = 0.12, seas = false },
  ice     = { temp = 0.16, moist = 0.55, seas = false },
  frost   = { temp = 0.12, moist = 0.40, seas = false },
  lava    = { temp = 0.95, moist = 0.05, seas = false },
  crystal = { temp = 0.45, moist = 0.25, seas = false },
  barren  = { temp = 0.40, moist = 0.02, seas = false },
}

function kindOf(kind)
  return KIND[kind] or KIND.barren
end

-- ── the world record ────────────────────────────────────────────────────────
-- Published by system_generator at roll time. Absent (an authored scene, an old
-- save) we still answer, from whatever the rails know — a body always has a
-- radius and a µ, and "barren, airless, no sea" is the honest default.

function worldOf(name)
  if not name then return nil end
  local w = save.get("world." .. name)
  if w then return w end
  local b = space.body(name)
  if not b then return nil end
  return { kind = "barren", seed = 1, radius = b.radius or 100, relief = 0,
           sea = 0, atmo = 0, insol = 1.0, name = name }
end

function seaRadius(name)
  local w = worldOf(name)
  return (w and w.sea) or 0
end

-- Does this world have air a plant could breathe? Flora needs SOMETHING; the
-- airless moons stay bare, which is what makes a living world land.
function habitable(name)
  local w = worldOf(name)
  if not w then return false end
  return (w.atmo or 0) > 0.15
end

-- ── the sample ──────────────────────────────────────────────────────────────

local function len(x, y, z)
  return math.sqrt(x * x + y * y + z * z)
end

-- Pure core, so the smoke harness can drive it without a scene: given the world
-- record and a BODY-RELATIVE position, what is it like here?
function sampleRel(w, rx, ry, rz)
  local r = len(rx, ry, rz)
  if r < 1e-6 then r = 1e-6 end
  local radius = w.radius or 100
  local relief = math.max(1.0, w.relief or (radius * 0.06))
  local base = kindOf(w.kind)
  local seed = (w.seed or 1) % 4096

  -- Latitude from the body's own polar axis (+Y in its local frame). Poles are
  -- cold, the equator is not — the one piece of planetary physics that reads
  -- instantly from the ground.
  local lat = math.abs(ry / r)                 -- 0 = equator, 1 = pole
  local sea = w.sea or 0
  local ground = r                              -- radius of this point
  local elev = (sea > 0) and (ground - sea) or (ground - radius)

  -- Insolation scales the archetype's base temperature (a canyon world close in
  -- is a desert; the same roll further out is a steppe).
  local t = base.temp * (0.55 + 0.45 * (w.insol or 1.0))
  t = t - lat * lat * 0.42                      -- polar cooling
  t = t - math.max(0, elev) / relief * 0.22     -- lapse rate with height
  -- A thick atmosphere evens the extremes out; a thin one lets them run.
  local atmo = w.atmo or 0
  t = 0.5 + (t - 0.5) * (1.0 - 0.30 * atmo)
  -- Regional weather: two octaves of the engine's own noise field, seeded per
  -- world so every planet's map of hot and cold is its own.
  local nx, ny, nz = rx / radius * 3.1, ry / radius * 3.1, rz / radius * 3.1
  t = t + math.noise(nx, ny, nz, seed) * 0.10
  t = math.max(0, math.min(1, t))

  local mo = base.moist * (0.5 + 0.5 * atmo)
  if sea > 0 then
    -- Near the sea is wet; a thousand metres above it is not. The falloff is in
    -- units of the world's own relief, so a flat world is wet all over and a
    -- mountainous one has a genuine rain shadow.
    mo = mo + 0.55 * math.max(0, 1.0 - math.max(0, elev) / (relief * 0.9))
  end
  mo = mo + math.noise(nz * 2.3 + 11.0, nx * 2.3, ny * 2.3, seed + 7) * 0.22
  mo = mo - math.max(0, (t - 0.75)) * 0.6       -- the hot places bake dry
  mo = math.max(0, math.min(1, mo))

  local biome = biomeFrom(w, t, mo, elev, relief, sea)
  return { biome = biome, temp = t, moist = mo, elev = elev, lat = lat,
           sea = sea, radius = radius, relief = relief, kind = w.kind,
           label = (BIOMES[biome] or BIOMES.barren).label,
           density = (BIOMES[biome] or BIOMES.barren).flora }
end

-- Temperature + moisture + height → biome. Order matters: water first (you are
-- either under the sea or you aren't), then the extremes, then the middle.
function biomeFrom(w, t, mo, elev, relief, sea)
  if (w.atmo or 0) <= 0.15 then
    -- Airless: the archetype IS the biome. Nothing grows, but the survey
    -- readout should still say something true about where you're standing.
    if w.kind == "lava" then return "ash" end
    if w.kind == "crystal" then return "crystal" end
    if w.kind == "ice" or w.kind == "frost" then return "snow" end
    return "barren"
  end
  if sea > 0 and elev < 0 then
    return elev > -relief * 0.18 and "reef" or "ocean"
  end
  if sea > 0 and elev < relief * 0.05 then return "shore" end
  if t < 0.18 then return "snow" end
  if t < 0.32 then return mo > 0.45 and "tundra" or "steppe" end
  if elev > relief * 0.72 then return "highland" end
  if t > 0.82 and mo < 0.35 then return "desert" end
  if mo > 0.72 then
    if t > 0.62 then return "jungle" end
    return t > 0.40 and "wetland" or "forest"
  end
  if mo > 0.45 then return t > 0.70 and "jungle" or "forest" end
  if mo > 0.24 then return "grass" end
  if t > 0.70 then return "desert" end
  return "steppe"
end

-- The scene-facing form: a WORLD position, resolved against the body it belongs
-- to. Returns nil off every world (deep space is not a climate).
function sampleAt(name, x, y, z)
  local w = worldOf(name)
  local b = name and space.body(name)
  if not (w and b) then return nil end
  return sampleRel(w, x - b.x, y - b.y, z - b.z)
end

-- ── water ───────────────────────────────────────────────────────────────────
-- One rule, used by the swimmer, the splashdown check and the flora scatter:
-- you are underwater when you are inside the sea sphere of the body you're at.

-- How deep below the surface of the sea, in metres. Negative = above it; nil
-- when this body has no sea (or there's no body).
function seaDepth(x, y, z)
  local d = space.dominant(x, y, z)
  if not d then return nil end
  local sea = seaRadius(d)
  if sea <= 0 then return nil end
  local b = space.body(d)
  if not b then return nil end
  return sea - len(x - b.x, y - b.y, z - b.z)
end

function isUnderwater(x, y, z)
  local d = seaDepth(x, y, z)
  return d ~= nil and d > 0
end

-- The outward normal of the water surface here — "up" for anything floating.
function upAt(x, y, z, name)
  local d = name or space.dominant(x, y, z)
  local b = d and space.body(d)
  if not b then return 0, 1, 0 end
  local dx, dy, dz = x - b.x, y - b.y, z - b.z
  local l = len(dx, dy, dz)
  if l < 1e-6 then return 0, 1, 0 end
  return dx / l, dy / l, dz / l
end

-- A one-line survey of where someone is standing — the HUD's location readout.
function surveyLine(x, y, z)
  local d = space.dominant(x, y, z)
  if not d then return nil end
  local s = sampleAt(d, x, y, z)
  if not s then return d end
  local depth = seaDepth(x, y, z)
  if depth and depth > 0 then
    return string.format("%s · %s · %.0f m down", d, s.label, depth)
  end
  return string.format("%s · %s · %.0f°C", d, s.label, -40 + s.temp * 90)
end
