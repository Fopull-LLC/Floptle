-- SYSTEM GENERATOR — game-side procgen tooling (NOT an engine feature).
-- Attach to an empty "System Generator" node; the Inspector shows a
-- ▶ Generate button (the --@editorButton below). Clicking it REPLACES the
-- generated bodies in the OPEN scene with a freshly rolled star system:
-- star class, planets (canyon/dune/ice/lava/crystal), moons, orbits,
-- atmospheres, caves, molten cores, names — then queues the heavy terrain
-- fills on the engine's background generator (watch the Console; save the
-- scene once they're in).
--
-- Every knob is a tweakable param below. seed 0 = a new system every click
-- (the rolled seed prints to the Console — put it in the seed param to
-- reproduce). Everything generated lives under ONE "<Star> System" group node
-- (tagged "gensystem"; bodies inside also tagged "genbody"), so regenerating
-- destroys the old system as a single subtree and never touches the rest of
-- the scene — and you can hand-delete a whole system the same way.
--
--@editorButton Generate generate

defaults = {
  seed = 0,          -- 0 = random every click; any number reproduces that system
  planets = 0,       -- 0 = random 2..4
  minOrbit = 5000,   -- first planet's orbit radius
  spacing = 2.2,     -- orbit ratio between neighbours (jittered ±15%)
  radiusMin = 100,   -- planet terrain radius range
  radiusMax = 230,
  gravityMin = 4.5,  -- surface gravity range (µ = g·r²)
  gravityMax = 10,
  moonChance = 0.6,  -- chance a planet gets each of up to 2 moons
  atmoChance = 0.8,  -- chance a planet has an atmosphere
  caveScale = 1.0,   -- multiplies cave-zone depth on every body
  starScale = 1.0,   -- multiplies the star's brightness
}

-- ---------------------------------------------------------------------------
-- helpers

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

local SYL_A = { "Ka", "Ve", "Zor", "Ath", "Or", "Ta", "Dra", "Pel", "Na", "Gol", "Bry", "Um", "Sol", "Ry" }
local SYL_B = { "ru", "il", "un", "mi", "quo", "os", "ver", "eth", "and", "ol", "ia", "ex" }

local function rollName(r, taken)
  while true do
    local n = r:pick(SYL_A) .. r:pick(SYL_B)
    if r:next() < 0.5 then n = n .. r:pick(SYL_B) end
    if not taken[n] then
      taken[n] = true
      return n
    end
  end
end

-- Archetypes: each returns the generatePlanet opts + an atmosphere hue.
-- Thresholds are tuned so caves read as LIT rock with glowing veins/pockets,
-- not fullbright walls (glow slots bypass lighting by design — use sparingly).
local function archetype(r, kind, radius, caveScale)
  local j = r:range(-0.03, 0.03)
  local caveK = ({ canyon = 0.35, dune = 0.25, ice = 0.3, lava = 0.35, crystal = 0.4,
    barren = 0.25, frost = 0.25 })[kind]
  local o = {
    radius = radius,
    relief = radius * r:range(0.05, 0.08),
    bumpFreq = r:range(3.8, 5.2),
    caveDepth = radius * caveK * caveScale,
    coreR = math.max(radius * 0.07, 5),
    corePaint = { slot = 6, color = { 0.98, 0.82, 0.6 } },
  }
  local atmoHue
  if kind == "canyon" then
    local warm, cool = r:range(0.01, 0.09) + j, r:range(0.68, 0.85) + j
    o.surfaceA = { slot = 1, color = hsv(warm, r:range(0.25, 0.45), r:range(0.6, 0.75)) }
    o.surfaceB = { slot = 2, color = hsv(cool, r:range(0.15, 0.3), r:range(0.55, 0.7)) }
    o.subsoil = { slot = 3, color = hsv(cool, 0.18, 0.62) }
    o.strata = { slot = 4, color = hsv(warm, 0.25, 0.75) }
    o.deep = { slot = 5, color = hsv(warm, 0.3, 0.68) }
    o.pockets = { slot = 7, color = hsv(r:range(0.7, 0.82), 0.28, 0.82), threshold = r:range(0.44, 0.5), minDepth = 8 }
    o.seam = { slot = 6, color = hsv(warm, 0.25, 0.9), minDepth = o.caveDepth * 0.5, center = 0.32, width = 0.045 }
    atmoHue = r:range(0.02, 0.1)
  elseif kind == "dune" then
    local gold = r:range(0.08, 0.15) + j
    o.bumpFreq = r:range(6.5, 8.5)
    o.relief = radius * r:range(0.06, 0.1)
    o.surfaceA = { slot = 2, color = hsv(gold, r:range(0.4, 0.55), r:range(0.85, 0.98)) }
    o.surfaceB = { slot = 1, color = hsv(gold - 0.03, r:range(0.4, 0.55), r:range(0.65, 0.8)) }
    o.patchBias = 0.6
    o.patchThr = -0.05
    o.subsoil = { slot = 4, color = hsv(gold - 0.02, 0.5, 0.85) }
    o.strata = { slot = 5, color = hsv(gold - 0.04, 0.5, 0.7) }
    o.deep = { slot = 5, color = hsv(gold - 0.05, 0.45, 0.62) }
    o.pockets = { slot = 6, color = hsv(gold, 0.4, 0.95), threshold = r:range(0.46, 0.52), minDepth = 10 }
    o.seam = { slot = 6, color = hsv(gold, 0.4, 1.0), minDepth = o.caveDepth * 0.6, center = 0.3, width = 0.04 }
    atmoHue = r:range(0.08, 0.13)
  elseif kind == "ice" then
    local blue = r:range(0.5, 0.62) + j
    o.bumpFreq = r:range(2.6, 3.6)
    o.relief = radius * r:range(0.04, 0.07)
    o.surfaceA = { slot = 12, color = hsv(blue, r:range(0.1, 0.25), r:range(0.85, 0.98)) }
    o.surfaceB = { slot = 9, color = hsv(blue, r:range(0.15, 0.3), r:range(0.7, 0.82)) }
    o.patchBias = 0.2
    o.patchThr = 0.25
    o.subsoil = { slot = 9, color = hsv(blue, 0.2, 0.72) }
    o.strata = { slot = 3, color = hsv(blue + 0.04, 0.25, 0.6) }
    o.deep = { slot = 9, color = hsv(blue, 0.25, 0.55) }
    o.pockets = { slot = 8, color = hsv(blue + r:range(-0.05, 0.05), 0.3, 0.9), threshold = r:range(0.4, 0.48), minDepth = 6 }
    o.iceCaps = { lat = 0.75, slot = 12, color = { 0.85, 0.92, 0.98 } }
    o.corePaint = { slot = 8, color = hsv(blue, 0.3, 0.9) }
    atmoHue = r:range(0.5, 0.6)
  elseif kind == "lava" then
    local ash = r:range(0.0, 0.06) + j
    o.bumpFreq = r:range(4.0, 5.5)
    o.surfaceA = { slot = 1, color = hsv(ash, r:range(0.15, 0.3), r:range(0.28, 0.4)) }
    o.surfaceB = { slot = 4, color = hsv(ash, r:range(0.2, 0.35), r:range(0.4, 0.52)) }
    o.patchBias = 0.4
    o.patchThr = 0.05
    o.subsoil = { slot = 4, color = hsv(ash, 0.25, 0.35) }
    o.strata = { slot = 5, color = hsv(ash, 0.3, 0.45) }
    o.deep = { slot = 5, color = hsv(ash, 0.3, 0.5) }
    o.pockets = { slot = 6, color = hsv(ash + 0.02, 0.5, 1.0), threshold = r:range(0.38, 0.44), minDepth = 6 }
    -- Surface cracks: shallow THIN seams — filaments of glow, not floods.
    o.seam = { slot = 6, color = hsv(ash + 0.02, 0.5, 1.0), minDepth = 6, center = 0.3, width = 0.05 }
    atmoHue = r:range(0.0, 0.04)
  elseif kind == "crystal" then
    local vio = (r:next() < 0.5 and r:range(0.68, 0.8) or r:range(0.45, 0.55)) + j
    o.surfaceA = { slot = 2, color = hsv(vio, r:range(0.2, 0.35), r:range(0.55, 0.7)) }
    o.surfaceB = { slot = 3, color = hsv(vio + 0.06, r:range(0.15, 0.3), r:range(0.45, 0.6)) }
    o.patchBias = 0.35
    o.patchThr = 0.0
    o.subsoil = { slot = 3, color = hsv(vio, 0.2, 0.55) }
    o.strata = { slot = 4, color = hsv(vio, 0.25, 0.5) }
    o.deep = { slot = 5, color = hsv(vio, 0.28, 0.6) }
    local slot = vio > 0.6 and 7 or 8
    o.pockets = { slot = slot, color = hsv(vio, 0.32, 0.95), threshold = r:range(0.32, 0.38), minDepth = 5 }
    o.corePaint = { slot = slot, color = hsv(vio, 0.32, 0.95) }
    atmoHue = r:range(0.55, 0.8)
  elseif kind == "frost" then
    local blue = r:range(0.5, 0.6) + j
    o.bumpFreq = r:range(2.6, 3.6)
    o.surfaceA = { slot = 12, color = hsv(blue, 0.12, 0.92) }
    o.surfaceB = { slot = 10, color = hsv(blue, 0.08, 0.72) }
    o.subsoil = { slot = 9, color = hsv(blue, 0.15, 0.7) }
    o.strata = { slot = 9, color = hsv(blue, 0.18, 0.6) }
    o.deep = { slot = 9, color = hsv(blue, 0.2, 0.55) }
    o.pockets = { slot = 8, color = hsv(blue, 0.3, 0.9), threshold = r:range(0.44, 0.5), minDepth = 4 }
    o.iceCaps = { lat = 0.6, slot = 12, color = { 0.8, 0.9, 0.95 } }
    o.craters = 8 + r:int(0, 9)
    o.corePaint = { slot = 8, color = hsv(blue, 0.3, 0.9) }
  else -- barren
    local g = r:range(0.5, 0.75)
    o.surfaceA = { slot = 10, color = hsv(r:next(), 0.04, g) }
    o.surfaceB = { slot = 11, color = hsv(0.1, 0.08, g + 0.1) }
    o.patchThr = -0.42
    o.subsoil = { slot = 9, color = { 0.6, 0.62, 0.68 } }
    o.strata = { slot = 9, color = { 0.55, 0.56, 0.6 } }
    o.deep = { slot = 9, color = { 0.5, 0.5, 0.55 } }
    o.pockets = { slot = 8, color = hsv(r:range(0.5, 0.8), 0.3, 0.9), threshold = r:range(0.46, 0.54), minDepth = 3 }
    o.craters = 8 + r:int(0, 9)
    o.corePaint = { slot = 8, color = { 0.55, 0.8, 1.0 } }
  end
  return o, atmoHue
end

-- One body under the system group: node + celestial + core kernel. The heavy
-- terrain fill is queued separately in generate() (it needs no node handle).
local function makeBody(sys, b)
  createNode(b.name, sys, function(n)
    n:setTerrain(b.id)
    n:setCelestial(b.cel)
    n.x, n.y, n.z = b.pos[1], b.pos[2], b.pos[3]
    n.tags = { "genbody" }
    local hot = (b.opts.corePaint and b.opts.corePaint.slot == 6)
    local cc = hot and { 1.0, 0.55, 0.25 } or { 0.55, 0.8, 1.0 }
    local ce = hot and { 1.0, 0.45, 0.15 } or { 0.4, 0.7, 1.0 }
    createNode(b.name .. " Core", n, function(core)
      core:setPrimitive("Sphere", cc)
      core:setMaterial { color = cc, emissive = ce, emissiveStrength = 2.5, unlit = true }
      core.scale = math.max(b.opts.coreR * 0.8, 4)
      core.tags = { "genbody" }
    end)
  end)
end

-- ---------------------------------------------------------------------------

function generate(node)
  local p = params
  local r = (p.seed > 0) and rng(p.seed) or rng()
  print(string.format("SYSTEM GENERATOR — seed %d (set the seed param to reproduce)", r.seed))

  -- Clear our previous work: the whole system GROUP goes in one subtree
  -- destroy (that's the point of generating into a clean hierarchy), plus any
  -- stray top-level "genbody" nodes from the old flat format.
  for _, n in ipairs(findTagged("gensystem")) do n:destroy() end
  for _, n in ipairs(findTagged("genbody")) do n:destroy() end

  local taken = {}

  -- The star: class → color + brightness; µ sets the year lengths.
  local classes = {
    { { 0.72, 0.82, 1.0 }, 90, 1 }, -- blue-white
    { { 1.0, 0.97, 0.9 }, 55, 2 },  -- white
    { { 1.0, 0.92, 0.6 }, 40, 3 },  -- yellow
    { { 1.0, 0.72, 0.42 }, 26, 4 }, -- orange
    { { 1.0, 0.5, 0.34 }, 15, 5 },  -- red dwarf
  }
  local cls = r:pick(classes)
  local starName = rollName(r, taken)
  local starMu = r:range(4e7, 1.1e8)
  -- Brightness is set for ~1x irradiance AT THE FIRST PLANET (irradiance =
  -- lum*1e6/d^2; overbright washes the whole sky white through bloom). The
  -- class nudges it: blue-white runs hot, red dwarfs dim.
  local a1 = p.minOrbit * r:range(0.9, 1.3)
  local classHeat = ({ 1.25, 1.1, 1.0, 0.85, 0.7 })[cls[3]]
  local starLum = (a1 * a1 / 1e6) * r:range(0.95, 1.25) * classHeat * p.starScale
  local starR = r:range(650, 1000)
  -- Bodies are ROLLED first (pure data, deterministic per seed) and BUILT after,
  -- all under one "<Star> System" group node — so the scene stays organized and
  -- the next generation removes the entire old system in one subtree destroy.
  local specs = {}

  local planetKinds = { "canyon", "dune", "ice", "lava", "crystal" }
  local moonKinds = { "barren", "frost", "crystal", "ice" }
  -- Cap 8: with G1 terrain residency (docs/galaxy-streaming-proposal.md) only
  -- the bodies near the camera hold RAM/GPU — the cap is now generation time
  -- and disk, not memory. Each body still pre-generates its field (~4-10 s).
  local nPlanets = (p.planets > 0) and math.min(p.planets, 8) or (2 + r:int(0, 2))
  local a = a1
  local id = 1
  local firstPos, firstR, firstRelief = nil, 0, 0
  local lastKind = nil
  print(string.format("star %s: lum %.0f — %d planet(s)", starName, starLum, nPlanets))

  for pi = 1, nPlanets do
    local kind
    repeat
      kind = (pi == 1) and r:pick({ "canyon", "dune", "ice" }) or r:pick(planetKinds)
    until kind ~= lastKind
    lastKind = kind
    local radius = r:range(p.radiusMin, p.radiusMax)
    local g = r:range(p.gravityMin, p.gravityMax)
    local mu = g * radius * radius
    local name = rollName(r, taken)
    local m0 = r:range(0, 6.2831853)
    local pos = { a * math.cos(m0), 0, a * math.sin(m0) }
    local opts, atmoHue = archetype(r, kind, radius, p.caveScale)
    opts.seed = r:int(1, 1e9)
    local bodyR = radius + math.max(radius * 0.035, 4)
    local laplace = a * (mu / starMu) ^ 0.4
    local cel = {
      mu = mu, bodyRadius = bodyR,
      soi = math.min(math.max(laplace, bodyR * 12), a * 0.3),
      parent = starName,
      a = a,
      -- The SPAWN planet flies a flat circle: the authored crew positions are
      -- computed from a circular t=0 orbit, and any e/i offset snaps the
      -- planet away from them on the first rails tick.
      e = (pi == 1) and 0 or r:range(0, 0.06),
      i = (pi == 1) and 0 or r:range(0, 0.14),
      m0 = m0,
    }
    if atmoHue and r:next() < p.atmoChance then
      cel.atmoColor = hsv(atmoHue, 0.5, 0.85)
      cel.atmoHeight = radius * r:range(0.25, 0.45)
      cel.atmoDensity = r:range(0.5, 0.85)
      cel.clouds = math.max(0, r:range(-0.25, 0.65))
    end
    specs[#specs + 1] = { name = name, id = id, pos = pos, cel = cel, opts = opts }
    terrain.generatePlanet(id, opts)
    print(string.format("  %s — %s, r %.0f, g %.1f, orbit %.0f", name, kind, radius, g, a))
    if pi == 1 then firstPos, firstR, firstRelief = pos, radius, opts.relief end
    id = id + 1

    -- Moons: up to two, orbits inside half the planet's SOI.
    local soi = cel.soi
    local ma = radius * r:range(5, 7)
    for _ = 1, 2 do
      if r:next() < p.moonChance and ma < soi * 0.45 then
        local mr = math.min(r:range(26, 58), radius * 0.35)
        local mmu = r:range(2, 5) * mr * mr
        while ma * (mmu / mu) ^ 0.4 < mr * 2.5 do mmu = mmu * 1.6 end
        local mname = rollName(r, taken)
        local mkind = r:pick(moonKinds)
        local mm0 = r:range(0, 6.2831853)
        local mpos = { pos[1] + ma * math.cos(mm0), 0, pos[3] + ma * math.sin(mm0) }
        local mopts = archetype(r, mkind, mr, p.caveScale)
        mopts.seed = r:int(1, 1e9)
        specs[#specs + 1] = { name = mname, id = id, pos = mpos, cel = {
          mu = mmu, bodyRadius = mr + math.max(mr * 0.035, 4), soi = 0,
          parent = name, a = ma, e = r:range(0, 0.04), i = r:range(0, 0.25), m0 = mm0,
        }, opts = mopts }
        terrain.generatePlanet(id, mopts)
        print(string.format("    moon %s — %s, r %.0f, orbit %.0f", mname, mkind, mr, ma))
        id = id + 1
        ma = ma * r:range(1.8, 2.4)
      end
    end

    a = a * p.spacing * r:range(0.85, 1.15)
  end

  -- Build the hierarchy: ONE group node holds the star and every body, so the
  -- Hierarchy panel reads as a single tidy system and regeneration (or a hand
  -- delete of the group) cleans up everything at once. The group sits at the
  -- origin with no rotation/scale — bodies' positions stay world numbers.
  createNode(starName .. " System", function(sys)
    sys.x, sys.y, sys.z = 0, 0, 0
    sys.tags = { "gensystem" }
    createNode(starName, sys, function(star)
      star:setPrimitive("Sphere", cls[1])
      star:setMaterial { color = cls[1], emissive = cls[1], emissiveStrength = 2.0, unlit = true }
      star:setCelestial {
        mu = starMu, bodyRadius = starR * 0.85, soi = 0,
        luminosity = starLum, starColor = cls[1],
      }
      star.x, star.y, star.z = 0, 0, 0
      star.scale = starR
      star.tags = { "genbody" }
    end)
    for _, b in ipairs(specs) do makeBody(sys, b) end
  end)

  -- Move the crew to the first planet's north pole (daylit side varies —
  -- summon the ship with L if it settles awkwardly).
  if firstPos then
    local sy = firstPos[2] + firstR + firstRelief
    local who = { { "Astronaut", 0, 6, 0 }, { "Camera 1", 0, 11, 8 }, { "Ship", 14, 4, 0 } }
    for _, w in ipairs(who) do
      local n = find(w[1])
      if n then
        n.x = firstPos[1] + w[2]
        n.y = sy + w[3]
        n.z = firstPos[3] + w[4]
      end
    end
  end

  print("bodies placed — terrain fills are generating in the background (Console shows progress). Save the scene when they land.")
end
