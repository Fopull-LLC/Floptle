-- FLORA GENERATOR — every planet grows its own plants.
--
-- Nothing here is authored. A world's seed rolls a handful of SPECIES: their
-- silhouette (how many segments the trunk has, how far it leans, how the
-- branches split and spiral), their palette, whether they glow, what they're
-- made of and what you get for cutting one down. The climate decides which
-- species a world gets at all — a jungle world rolls broad canopies and
-- creepers, an ice world rolls low cushions and frost spires, an ocean rolls
-- kelp — and then the seed makes those particular ones unlike any other
-- planet's. Two saves never share a forest.
--
-- Plants are built from PRIMITIVES (cube / sphere / capsule) at retro fidelity:
-- a trunk is a stack of tapering boxes, a canopy is a cluster of spheres, a
-- frond is a splayed capsule. That's the look the rest of the game is drawn at,
-- and it costs nothing to generate at runtime — no meshes to author, no atlas
-- to pack, and a species is ~40 numbers.
--
-- Geometry convention: every plant is built in its ROOT's local frame, where
-- +Y is the surface normal. A direction is carried as (tilt, spin) — tilt from
-- +Y, spin about it — which is exactly the node's (pitch, yaw), so placing a
-- segment is one call and no matrices.
--
--     local fg = findScript("flora_gen")
--     local list = fg.speciesFor("Golil")          -- deterministic per world
--     fg.build(planetNode, sp, x,y,z, ux,uy,uz, n, "full", function(root) … end)
--
-- LIMITS (interim, and honest): these are ordinary scene nodes, so they have no
-- colliders — you walk through a tree — and a few hundred is the budget rather
-- than a few thousand. Real GPU-instanced scatter with collision and a harvest
-- hook is an engine feature (floptle/0036); `flora_field` keeps the node count
-- inside what the scene graph is comfortable with until it lands.

defaults = {}

-- Primitive dimensions at scale 1 (matter_catalog): a cube is 1.4 across, a
-- sphere is 1.7 across, a capsule is 1.0 across and 2.0 tall.
local P_CUBE, P_SPHERE, P_CAP_R, P_CAP_H = 1.4, 1.7, 1.0, 2.0

local function clamp(v, lo, hi) return math.max(lo, math.min(hi, v)) end

local function hsv(h, s, v)
  h = (h % 1) * 6
  local i = math.floor(h)
  local f = h - i
  local p, q, t = v * (1 - s), v * (1 - s * f), v * (1 - s * (1 - f))
  if i == 0 then return { v, t, p } end
  if i == 1 then return { q, v, p } end
  if i == 2 then return { p, v, t } end
  if i == 3 then return { p, q, v } end
  if i == 4 then return { t, p, v } end
  return { v, p, q }
end

-- ── naming ──────────────────────────────────────────────────────────────────
-- Species names are rolled from the world's own syllables so a planet's flora
-- sounds like it comes from one place, with a form noun you can act on
-- ("cut the ashen spires") rather than a catalogue number.

local SYL = { "vel", "quor", "ash", "mira", "tesk", "olun", "brae", "sith", "yuma",
              "kelv", "duno", "arri", "phos", "ulm", "cren", "sova", "thal", "ixi" }
local NOUN = {
  tree      = { "Bole", "Mast", "Spar", "Crown", "Wood" },
  shrub     = { "Bramble", "Bush", "Thicket", "Scrub" },
  grass     = { "Reed", "Sedge", "Tuft", "Blade" },
  fungus    = { "Cap", "Bloom", "Shroom", "Puffball" },
  spire     = { "Spire", "Shard", "Needle", "Prism" },
  succulent = { "Bulb", "Cushion", "Barrel", "Pad" },
  kelp      = { "Kelp", "Weed", "Frond", "Ribbon" },
  vine      = { "Creeper", "Vine", "Coil", "Snare" },
}

local function rollName(r, form)
  local n = r:pick(SYL)
  if r:next() < 0.6 then n = n .. r:pick(SYL) end
  n = n:sub(1, 1):upper() .. n:sub(2)
  return n .. " " .. r:pick(NOUN[form] or NOUN.shrub)
end

-- ── what each form is made of ───────────────────────────────────────────────
-- Yields are per plant, in inventory units. A tree is worth walking to; a tuft
-- of grass is worth the two seconds it costs. Bioluminescent variants add glow
-- sap, which is where the money in xenobotany actually is.

local FORM = {
  tree      = { work = 3.4, yields = { { "timber", 3, 6 }, { "fiber", 1, 2 } } },
  shrub     = { work = 1.6, yields = { { "fiber", 2, 4 }, { "biomass", 1, 2 } } },
  grass     = { work = 0.6, yields = { { "fiber", 1, 2 } } },
  fungus    = { work = 1.2, yields = { { "biomass", 2, 3 }, { "spores", 1, 2 } } },
  spire     = { work = 2.6, yields = { { "crystal", 1, 2 }, { "silica", 1, 3 } } },
  succulent = { work = 1.4, yields = { { "water", 2, 3 }, { "biomass", 1, 1 } } },
  kelp      = { work = 1.5, yields = { { "kelp", 2, 4 }, { "biomass", 1, 2 } } },
  vine      = { work = 1.8, yields = { { "fiber", 3, 5 }, { "resin", 1, 2 } } },
}

-- Which forms a biome can grow, and how likely each is. This is the climate's
-- whole say in the matter: it picks the CAST, the seed writes the characters.
local BIOME_FORMS = {
  jungle   = { tree = 4, vine = 3, shrub = 2, fungus = 2, grass = 2 },
  forest   = { tree = 5, shrub = 3, fungus = 2, grass = 2 },
  wetland  = { tree = 2, shrub = 3, grass = 4, fungus = 3 },
  grass    = { grass = 5, shrub = 3, tree = 1 },
  steppe   = { grass = 4, shrub = 2, succulent = 1 },
  shore    = { grass = 3, shrub = 2, succulent = 2 },
  reef     = { kelp = 5, fungus = 2 },
  ocean    = { kelp = 4 },
  desert   = { succulent = 4, spire = 2, shrub = 1 },
  tundra   = { shrub = 3, grass = 2, fungus = 2 },
  snow     = { spire = 2, shrub = 1 },
  highland = { shrub = 3, grass = 2, spire = 1 },
  ash      = { spire = 2, fungus = 1 },
  crystal  = { spire = 5, fungus = 1 },
  barren   = {},
}

function formsFor(biome)
  return BIOME_FORMS[biome] or {}
end

-- ── the species roll ────────────────────────────────────────────────────────

local cache = {}   -- world name → species list (deterministic; rolled once)

-- One species. `r` is the world's stream, so the whole set is reproducible.
local function rollSpecies(r, form, w, palette)
  local sp = { form = form, name = rollName(r, form) }
  local hot = (w.temp or 0.5) > 0.65
  local cold = (w.temp or 0.5) < 0.3

  -- SILHOUETTE. Every number here is a knob the seed turns, and between them
  -- they cover everything from a mushroom to a 14 m mast.
  local h = ({ tree = r:range(4.5, 11.0), shrub = r:range(1.0, 2.2),
               grass = r:range(0.5, 1.3), fungus = r:range(0.6, 1.8),
               spire = r:range(1.6, 5.5), succulent = r:range(0.5, 1.6),
               kelp = r:range(2.5, 7.0), vine = r:range(1.4, 3.2) })[form] or 1.5
  sp.height = h
  sp.vary = r:range(0.18, 0.45)                    -- ± size between individuals
  sp.segs = (form == "tree") and r:int(2, 4) or ((form == "kelp") and r:int(3, 5) or r:int(1, 2))
  sp.taper = r:range(0.55, 0.85)                   -- radius kept per segment
  sp.girth = r:range(0.045, 0.12) * (form == "tree" and 1.0 or 0.7)
  sp.lean = r:range(0.0, 0.30)                     -- radians of sway per segment
  sp.spin = r:range(0.0, 2.4)                      -- azimuth walk per segment
  sp.stem = r:pick({ "Cube", "Cube", "Capsule" })  -- boxes read more retro
  sp.branches = (form == "tree") and r:int(2, 5)
    or ((form == "shrub" or form == "vine") and r:int(2, 4) or 0)
  sp.branchAngle = r:range(0.35, 1.15)             -- from the trunk
  sp.branchLen = r:range(0.25, 0.55)               -- of the plant's height
  sp.branchUp = r:range(0.4, 1.0)                  -- fraction of trunk where they start
  sp.spiral = r:range(1.6, 2.9)                    -- golden-ish, but not exactly

  -- FOLIAGE. Shape, count, size and droop. A "leaf" is a blob, a slab or a
  -- splayed capsule — which one is the biggest single difference between two
  -- planets' trees.
  sp.leaf = r:pick({ "Sphere", "Sphere", "Cube", "Capsule" })
  sp.leaves = ({ tree = r:int(3, 7), shrub = r:int(3, 6), grass = r:int(2, 5),
                 fungus = 1, spire = 0, succulent = r:int(2, 5),
                 kelp = r:int(3, 6), vine = r:int(3, 6) })[form] or 3
  sp.leafSize = r:range(0.22, 0.62)
  sp.leafSpread = r:range(0.25, 0.85)
  sp.leafDroop = r:range(-0.2, 0.9)
  sp.flat = r:range(0.35, 1.0)                     -- canopy squash (1 = round)

  -- PALETTE. Every species is a variation on the WORLD's hue, so a planet has a
  -- coherent look while each plant is still its own — and cold worlds drift
  -- blue, hot ones ochre, exactly as the eye expects before it knows why.
  local dh = r:range(-palette.spread, palette.spread)
  local leafH = palette.hue + dh
  local sat = clamp(palette.sat + r:range(-0.18, 0.18), 0.15, 0.95)
  local val = clamp(palette.val + r:range(-0.18, 0.18), 0.2, 1.0)
  sp.leafColor = hsv(leafH, sat, val)
  sp.leafColor2 = hsv(leafH + r:range(-0.06, 0.06), clamp(sat + 0.1, 0, 1),
                      clamp(val * r:range(0.7, 1.15), 0.1, 1.0))
  local barkH = palette.hue + (cold and 0.45 or 0.05) + r:range(-0.05, 0.05)
  sp.stemColor = hsv(barkH, r:range(0.15, 0.45), r:range(0.22, 0.6))
  if form == "spire" then
    sp.stemColor = sp.leafColor
  end

  -- GLOW. Rare on a sunlit world, common in the dark and the deep — and worth
  -- real money when it happens (glow sap).
  local glowChance = (w.insol or 1) < 0.6 and 0.45 or 0.16
  if form == "fungus" or form == "spire" then glowChance = glowChance + 0.25 end
  sp.glow = (r:next() < glowChance) and r:range(0.6, 2.4) or 0
  sp.glowColor = hsv(leafH + r:range(-0.12, 0.12), clamp(sat + 0.25, 0, 1), 1.0)

  -- WHAT IT'S MADE OF. The form sets the staples; a glowing species carries sap
  -- and a hot-world one carries resin, so where you harvest matters.
  local f = FORM[form] or FORM.shrub
  sp.work = f.work
  sp.yields = {}
  for _, y in ipairs(f.yields) do
    sp.yields[#sp.yields + 1] = { mat = y[1], min = y[2], max = y[3] }
  end
  if sp.glow > 0 then
    sp.yields[#sp.yields + 1] = { mat = "glowsap", min = 1, max = 2 }
  end
  if hot and form ~= "spire" and r:next() < 0.4 then
    sp.yields[#sp.yields + 1] = { mat = "resin", min = 1, max = 2 }
  end

  -- Where it grows. A species claims the biomes its form is offered in, with a
  -- preference — the same forest can be two species deep with one dominant.
  sp.biomes = {}
  for biome, forms in pairs(BIOME_FORMS) do
    if forms[form] then sp.biomes[biome] = forms[form] * r:range(0.5, 1.5) end
  end
  sp.aquatic = (form == "kelp")
  return sp
end

-- Every species a world grows, rolled once and cached. Deterministic: the same
-- world record always produces the same flora, which is what lets the scatter
-- field rebuild a forest you walked away from.
function speciesFor(worldName, w)
  if cache[worldName] then return cache[worldName] end
  local cl = findScript("climate")
  w = w or (cl and cl.worldOf(worldName))
  if not w then return {} end
  local list = {}
  if (w.atmo or 0) > 0.15 or (w.sea or 0) > 0 then
    local r = rng((w.seed or 1) * 7919 % 2147483647 + 13)
    -- The world's palette: hue from its warmth (cold worlds run blue-violet,
    -- temperate green, hot ochre-red) plus a per-world rotation so no two
    -- temperate worlds are the same green.
    local temp = w.temp or 0.5
    local baseHue = (temp < 0.3) and r:range(0.44, 0.62)
      or ((temp > 0.72) and r:range(0.02, 0.14) or r:range(0.18, 0.40))
    local palette = { hue = baseHue + r:range(-0.05, 0.05),
                      spread = r:range(0.04, 0.16),
                      sat = r:range(0.35, 0.8),
                      val = r:range(0.45, 0.85) }
    w.temp = temp
    -- Which forms this world can grow at all = the union of its plausible
    -- biomes' cast lists. We don't know every biome the surface holds without
    -- walking it, so the roll takes the world's character: seas add kelp,
    -- warmth adds canopies, cold takes them away.
    local pool = {}
    local function offer(form, n) pool[form] = (pool[form] or 0) + n end
    if (w.sea or 0) > 0 then offer("kelp", 2) end
    if (w.atmo or 0) > 0.15 then
      offer("grass", 2); offer("shrub", 2); offer("fungus", 1)
      if temp > 0.28 and temp < 0.85 then offer("tree", 3) end
      if temp > 0.6 then offer("vine", 1); offer("succulent", 1) end
      if temp < 0.35 then offer("spire", 1) end
      if temp > 0.7 then offer("succulent", 1) end
    end
    if w.kind == "crystal" then offer("spire", 2) end
    local forms = {}
    for form, n in pairs(pool) do
      for _ = 1, n do forms[#forms + 1] = form end
    end
    table.sort(forms)
    local n = math.min(#forms, 4 + r:int(0, 3))
    local used = {}
    for i = 1, n do
      local form = forms[1 + r:int(0, #forms - 1)]
      -- At most two species per form: variety across shapes beats six
      -- near-identical trees.
      if (used[form] or 0) < 2 then
        used[form] = (used[form] or 0) + 1
        local sp = rollSpecies(r, form, w, palette)
        sp.id = #list + 1
        list[#list + 1] = sp
      end
    end
  end
  cache[worldName] = list
  return list
end

-- Which species suit a biome, with weights. The scatter field rolls against
-- this; an empty answer means nothing grows here, which is a real answer.
function speciesIn(worldName, biome)
  local out = {}
  for _, sp in ipairs(speciesFor(worldName)) do
    local w = sp.biomes[biome]
    if w and w > 0 then out[#out + 1] = { sp = sp, w = w } end
  end
  return out
end

-- ── building a plant ────────────────────────────────────────────────────────

-- A direction carried as (tilt from +Y, spin about it) IS a node's (pitch, yaw).
local function dir_of(tilt, spin)
  local st = math.sin(tilt)
  return st * math.sin(spin), math.cos(tilt), st * math.cos(spin)
end

-- One primitive: a segment of length L and radius R starting at `p` and running
-- along (tilt, spin). Returns the tip, so segments chain.
local function segment(parent, shape, color, glow, gcol, px, py, pz, tilt, spin, L, R, name)
  local dx, dy, dz = dir_of(tilt, spin)
  local cx, cy, cz = px + dx * L * 0.5, py + dy * L * 0.5, pz + dz * L * 0.5
  createNode(name, parent, function(n)
    n:setPrimitive(shape, color)
    local m = { color = color, ambient = 0.85 }
    if glow and glow > 0 then
      m.emissive = gcol
      m.emissiveStrength = glow
    end
    n:setMaterial(m)
    n.x, n.y, n.z = cx, cy, cz
    n.yaw, n.pitch, n.roll = spin, tilt, 0
    if shape == "Capsule" then
      n.scale_x, n.scale_z = R * 2 / P_CAP_R, R * 2 / P_CAP_R
      n.scale_y = L / P_CAP_H
    elseif shape == "Sphere" then
      n.scale_x, n.scale_z = R * 2 / P_SPHERE, R * 2 / P_SPHERE
      n.scale_y = L / P_SPHERE
    else
      n.scale_x, n.scale_z = R * 2 / P_CUBE, R * 2 / P_CUBE
      n.scale_y = L / P_CUBE
    end
  end)
  return px + dx * L, py + dy * L, pz + dz * L
end

-- A foliage blob at a point: the canopy, the cap, the frond cluster.
local function blob(parent, sp, r, px, py, pz, size, name)
  local shape = sp.leaf
  local col = (r:next() < 0.5) and sp.leafColor or sp.leafColor2
  createNode(name, parent, function(n)
    n:setPrimitive(shape, col)
    local m = { color = col, ambient = 0.9 }
    if sp.glow > 0 then
      m.emissive = sp.glowColor
      m.emissiveStrength = sp.glow
    end
    n:setMaterial(m)
    n.x, n.y, n.z = px, py, pz
    n.yaw = r:range(0, 6.283)
    n.pitch = r:range(-0.5, 0.5) * sp.leafDroop
    local base = (shape == "Sphere") and P_SPHERE or ((shape == "Capsule") and P_CAP_R or P_CUBE)
    n.scale_x = size * 2 / base
    n.scale_z = size * 2 / base
    n.scale_y = (size * 2 * sp.flat) / ((shape == "Capsule") and P_CAP_H or base)
  end)
end

-- Vessel/prefab basis for up alignment (same solve base_facilities uses): with
-- yaw free, R = Ry(yaw)·Rx(pitch)·Rz(roll) takes +Y to the surface normal.
local function up_angles(ux, uy, uz)
  local roll = math.asin(clamp(-ux, -1, 1))
  local pitch = math.atan2(uz, uy)
  return pitch, roll
end

-- Build one plant. `parent` is the body node (so the plant rides the planet's
-- orbit through the hierarchy), `x,y,z` is its position IN THAT PARENT'S frame,
-- and `ux,uy,uz` is the surface normal in the same frame. `detail` is "full" or
-- "far" — far plants are a silhouette, which is all you can read at 60 m and a
-- tenth of the nodes. `onRoot` receives the root handle once the engine has
-- made it (next pass — createNode is queued, like spawn).
function build(parent, sp, x, y, z, ux, uy, uz, seedn, detail, onRoot)
  local r = rng(seedn)
  local scale = 1.0 + r:range(-sp.vary, sp.vary)
  local h = sp.height * scale
  local pitch, roll = up_angles(ux, uy, uz)
  local far = (detail == "far")

  createNode("Flora", parent, function(root)
    root.x, root.y, root.z = x, y, z
    root.pitch, root.roll = pitch, roll
    -- The up-alignment solve assumes yaw = 0 (R = Ry(yaw)·Rx(pitch)·Rz(roll)
    -- takes +Y to the normal only then) — a random yaw here would rotate the
    -- plant about WORLD Y and tip it off the surface. Per-individual spin lives
    -- INSIDE the plant instead: the trunk starts at a rolled azimuth.
    root.yaw = 0
    root.tags = { "flora" }
    if onRoot then onRoot(root) end

    local girth = sp.girth * h
    if far then
      -- The silhouette: one stem, one crown. At distance that's the entire
      -- readable content of a tree.
      local tx, ty, tz = segment(root, sp.stem, sp.stemColor, 0, nil,
        0, 0, 0, 0, 0, h * 0.6, girth, "Stem")
      if sp.leaves > 0 then
        blob(root, sp, r, tx, ty, tz, math.max(h * 0.22, sp.leafSize * scale), "Crown")
      end
      return
    end

    -- The trunk: a chain of tapering segments that lean and spiral, so no two
    -- individuals stand the same way even within a species.
    local px, py, pz = 0, 0, 0
    local tilt, spin = 0, r:range(0, 6.283)
    local segs = math.max(1, sp.segs)
    local trunkH = h * ((sp.form == "tree") and 0.66 or 0.85)
    local joints = { { 0, 0, 0, 0, spin } }
    local R = girth
    for i = 1, segs do
      local L = trunkH / segs
      px, py, pz = segment(root, sp.stem, sp.stemColor, sp.glow * 0.35, sp.glowColor,
        px, py, pz, tilt, spin, L, R, "Seg" .. i)
      tilt = tilt + sp.lean * r:range(0.3, 1.0)
      spin = spin + sp.spin * r:range(0.5, 1.0)
      R = R * sp.taper
      joints[#joints + 1] = { px, py, pz, tilt, spin }
    end

    -- Branches leave the upper trunk on a spiral, each carrying foliage at its
    -- tip. `branchUp` decides whether the plant is a mast with a crown or a
    -- bush that splits at the ankle.
    local tips = { { px, py, pz } }
    for b = 1, (sp.branches or 0) do
      local f = sp.branchUp + (1 - sp.branchUp) * ((b - 0.5) / math.max(1, sp.branches))
      local j = joints[clamp(math.floor(f * segs) + 1, 1, #joints)]
      local bl = h * sp.branchLen * r:range(0.7, 1.2)
      local btilt = sp.branchAngle * r:range(0.75, 1.25)
      local bspin = (j[5] or 0) + b * sp.spiral + r:range(-0.3, 0.3)
      local ex, ey, ez = segment(root, sp.stem, sp.stemColor, sp.glow * 0.35, sp.glowColor,
        j[1], j[2], j[3], btilt, bspin, bl, R * 0.8, "Branch" .. b)
      tips[#tips + 1] = { ex, ey, ez }
    end

    -- Foliage: spread over the branch tips (and the crown), jittered by the
    -- species' own spread. A spire species has none — it IS its geometry.
    for i = 1, (sp.leaves or 0) do
      local t = tips[1 + (i - 1) % #tips]
      local s = sp.leafSize * scale * r:range(0.7, 1.3)
      local j = sp.leafSpread * h * 0.12
      blob(root, sp, r,
        t[1] + r:range(-j, j), t[2] + r:range(-j * 0.6, j), t[3] + r:range(-j, j),
        s, "Leaf" .. i)
    end
  end)
end

-- What cutting one down gives you, rolled per individual so a stand of trees
-- isn't six identical hauls.
function harvestYield(sp, seedn)
  local r = rng(seedn + 4919)
  local out = {}
  for _, y in ipairs(sp.yields or {}) do
    local n = y.min + r:int(0, math.max(0, y.max - y.min))
    if n > 0 then out[#out + 1] = { mat = y.mat, n = n } end
  end
  return out
end

-- The pocket-guide line a survey / the harvest prompt shows.
function describe(sp)
  local mats = findScript("materials")
  local parts = {}
  for _, y in ipairs(sp.yields or {}) do
    parts[#parts + 1] = (mats and mats.name(y.mat)) or y.mat
  end
  return string.format("%s — %s%s", sp.name, sp.form,
    #parts > 0 and ("  ·  " .. table.concat(parts, ", ")) or "")
end
