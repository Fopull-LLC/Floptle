-- MATERIALS — the registry every harvested, mined, hauled and sold thing is
-- weighed and valued by. One table, one truth: the inventory weighs a unit
-- through here, the depot prices it through here, and the tool that produced
-- it only ever names an id.
--
-- A unit is "one handful you can carry" — the numbers are per unit, and mass is
-- what actually limits you (a suit carries ~45 kg; a cargo hold hundreds).
-- Value is what the depot pays at reputation 0; standing moves the price, the
-- registry doesn't.
--
--     local M = findScript("materials")
--     M.name("iron")            "Iron Ore"
--     M.massOf("iron", 12)      kg for twelve units
--     M.pickOre("canyon", 18.5, r)   what this planet yields at that depth
--
-- WHERE things come from is registry data too (`BANDS`): rock yields by planet
-- archetype and depth, so a canyon world's shallow dirt is iron and silica while
-- uranite only ever shows up 25 m down. Flora yields live with the species that
-- grow them (`flora_gen`), because those are rolled per planet.

defaults = {}

-- id → { name, kg (per unit), value ($/unit at rep 0), tier, class }
-- class: "ore" | "ice" | "flora" | "liquid" | "refined"
REG = {
  regolith = { name = "Regolith",         kg = 1.8, value = 1,  tier = 0, class = "ore" },
  silica   = { name = "Silica",           kg = 1.6, value = 6,  tier = 1, class = "ore" },
  iron     = { name = "Iron Ore",         kg = 2.6, value = 9,  tier = 1, class = "ore" },
  copper   = { name = "Copper Ore",       kg = 2.4, value = 14, tier = 1, class = "ore" },
  ice      = { name = "Water Ice",        kg = 1.0, value = 4,  tier = 1, class = "ice" },
  water    = { name = "Water",            kg = 1.0, value = 3,  tier = 1, class = "liquid" },
  sulfur   = { name = "Sulfur",           kg = 1.4, value = 11, tier = 2, class = "ore" },
  obsidian = { name = "Obsidian",         kg = 2.2, value = 19, tier = 2, class = "ore" },
  crystal  = { name = "Resonant Crystal", kg = 1.1, value = 46, tier = 3, class = "ore" },
  uranium  = { name = "Uranite",          kg = 3.2, value = 85, tier = 3, class = "ore" },

  timber   = { name = "Timber",           kg = 1.2, value = 7,  tier = 1, class = "flora" },
  fiber    = { name = "Plant Fiber",      kg = 0.3, value = 5,  tier = 1, class = "flora" },
  biomass  = { name = "Biomass",          kg = 0.6, value = 3,  tier = 1, class = "flora" },
  resin    = { name = "Resin",            kg = 0.9, value = 18, tier = 2, class = "flora" },
  spores   = { name = "Spore Pods",       kg = 0.4, value = 26, tier = 2, class = "flora" },
  kelp     = { name = "Kelp",             kg = 0.8, value = 12, tier = 2, class = "flora" },
  glowsap  = { name = "Glow Sap",         kg = 0.7, value = 40, tier = 3, class = "flora" },
}

-- Rock yields per planet archetype. `w` is the raw weight; `minDepth` gates a
-- material to below that depth (metres under the surface) and `deep` multiplies
-- its weight the further down you are, so the interesting stuff is genuinely
-- worth the tunnel. Every list keeps regolith as the always-available filler —
-- there is no such thing as a dig that yields nothing.
BANDS = {
  canyon = {
    { mat = "regolith", w = 6.0 },
    { mat = "silica",   w = 2.5 },
    { mat = "iron",     w = 3.0, minDepth = 2,  deep = 1.4 },
    { mat = "copper",   w = 1.4, minDepth = 6,  deep = 1.6 },
    { mat = "crystal",  w = 0.3, minDepth = 20, deep = 2.0 },
    { mat = "uranium",  w = 0.15, minDepth = 25, deep = 2.2 },
  },
  dune = {
    { mat = "regolith", w = 7.0 },
    { mat = "silica",   w = 4.5 },
    { mat = "iron",     w = 1.8, minDepth = 4,  deep = 1.4 },
    { mat = "sulfur",   w = 1.2, minDepth = 8,  deep = 1.5 },
    { mat = "copper",   w = 0.8, minDepth = 12, deep = 1.6 },
    { mat = "uranium",  w = 0.1,  minDepth = 28, deep = 2.2 },
  },
  ice = {
    { mat = "ice",      w = 7.0 },
    { mat = "regolith", w = 2.0 },
    { mat = "silica",   w = 1.2, minDepth = 3 },
    { mat = "iron",     w = 1.0, minDepth = 10, deep = 1.5 },
    { mat = "crystal",  w = 0.4, minDepth = 22, deep = 2.0 },
  },
  frost = {
    { mat = "ice",      w = 6.0 },
    { mat = "regolith", w = 3.0 },
    { mat = "iron",     w = 1.2, minDepth = 8,  deep = 1.5 },
    { mat = "copper",   w = 0.6, minDepth = 14, deep = 1.6 },
  },
  lava = {
    { mat = "regolith", w = 4.0 },
    { mat = "obsidian", w = 3.0 },
    { mat = "sulfur",   w = 2.6 },
    { mat = "iron",     w = 2.4, minDepth = 3,  deep = 1.5 },
    { mat = "copper",   w = 1.6, minDepth = 8,  deep = 1.7 },
    { mat = "uranium",  w = 0.35, minDepth = 18, deep = 2.4 },
  },
  crystal = {
    { mat = "regolith", w = 3.5 },
    { mat = "silica",   w = 3.0 },
    { mat = "crystal",  w = 2.2, deep = 2.0 },
    { mat = "copper",   w = 1.2, minDepth = 6 },
    { mat = "uranium",  w = 0.3, minDepth = 20, deep = 2.2 },
  },
  barren = {
    { mat = "regolith", w = 7.0 },
    { mat = "silica",   w = 2.0 },
    { mat = "iron",     w = 2.0, minDepth = 3, deep = 1.4 },
    { mat = "copper",   w = 0.7, minDepth = 10, deep = 1.6 },
    { mat = "uranium",  w = 0.2, minDepth = 22, deep = 2.2 },
  },
}

function byId(id)
  return REG[id]
end

function name(id)
  local d = REG[id]
  return d and d.name or tostring(id)
end

function kg(id)
  local d = REG[id]
  return d and d.kg or 1.0
end

function value(id)
  local d = REG[id]
  return d and d.value or 0
end

function tier(id)
  local d = REG[id]
  return d and d.tier or 0
end

function class(id)
  local d = REG[id]
  return d and d.class or "ore"
end

function massOf(id, n)
  return kg(id) * (n or 0)
end

function valueOf(id, n)
  return value(id) * (n or 0)
end

-- Every id, ordered the one way panels list them: tier, then name. Stable —
-- a warehouse readout must never reshuffle between frames.
function list()
  local out = {}
  for id in pairs(REG) do out[#out + 1] = id end
  table.sort(out, function(a, b)
    local ta, tb = REG[a].tier, REG[b].tier
    if ta ~= tb then return ta < tb end
    return REG[a].name < REG[b].name
  end)
  return out
end

-- What a dig at `depth` metres under an archetype's surface turns up. `roll` is
-- a number in [0,1) — pass a seeded one (rng) and the same hole always yields
-- the same thing, which is what makes a rich seam a PLACE rather than a
-- slot machine you can stand still and pull.
function pickOre(kind, depth, roll)
  local bands = BANDS[kind] or BANDS.barren
  depth = math.max(0, depth or 0)
  local total, weights = 0, {}
  for i, b in ipairs(bands) do
    local w = 0
    if depth >= (b.minDepth or 0) then
      w = b.w
      if b.deep then
        -- Weight climbs with depth past the gate, saturating at 3× so a deep
        -- shaft is richer without ever making regolith impossible.
        local over = (depth - (b.minDepth or 0)) / 20.0
        w = w * (1.0 + math.min(2.0, over * (b.deep - 1.0) * 2.0))
      end
    end
    weights[i] = w
    total = total + w
  end
  if total <= 0 then return "regolith" end
  local pick = (roll or 0) * total
  for i, b in ipairs(bands) do
    pick = pick - weights[i]
    if pick <= 0 then return b.mat end
  end
  return bands[1].mat
end

-- The materials an archetype can produce at all — used by the survey readout
-- and by contracts that ask for something specific.
function oresOf(kind)
  local out = {}
  for _, b in ipairs(BANDS[kind] or BANDS.barren) do out[#out + 1] = b.mat end
  return out
end
