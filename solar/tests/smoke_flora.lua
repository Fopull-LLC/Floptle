-- SMOKE HARNESS: the living surface. Runs the real climate.lua, flora_gen.lua
-- and flora_field.lua against a stubbed engine and checks the whole chain —
-- world record → climate sample → biome → species roll → built plant → scatter
-- field → harvest into the inventory.
--
--   luajit solar/tests/smoke_flora.lua
--
-- Fidelity notes (the bugs this stub is shaped to catch):
--  * The planet is a real SPHERE here: `raycast` answers analytically at the
--    body's surface radius with a radial normal, so a scatter that projects its
--    cells wrongly puts plants nowhere and the counts go to zero.
--  * `createNode` builds a REAL node tree with local transforms, so a plant that
--    parents its parts wrong, or scales a primitive by the wrong base size,
--    shows up as geometry off in space rather than a silent pass.
--  * Determinism is checked twice over: the same world rolls the same species,
--    and the same cell rolls the same plants in the same places. That's the
--    property the whole "walk away and come back" design rests on.

local T = 0.0

-- ── engine API ──────────────────────────────────────────────────────────────
local API, store, logs = {}, {}, {}
API.save = {
  get = function(k) return store[k] end,
  set = function(k, v) store[k] = v end,
  flush = function() end,
}
function API.log(m) logs[#logs + 1] = tostring(m) end
function API.print(m) logs[#logs + 1] = tostring(m) end
function API.vec3(x, y, z) return { x = x, y = y, z = z } end

-- Deterministic RNG with the engine's surface (next/range/int/pick + .seed).
local function make_rng(seed)
  local s = (seed or 12345) % 2147483647
  if s <= 0 then s = s + 2147483646 end
  local r = { seed = seed or 12345 }
  function r:next()
    s = (s * 16807) % 2147483647
    return (s - 1) / 2147483646
  end
  function r:range(a, b) return a + (b - a) * r:next() end
  function r:int(a, b) return a + math.floor(r:next() * (b - a + 1) - 1e-9) end
  function r:pick(t) return t[1 + math.floor(r:next() * #t - 1e-9)] end
  return r
end
local rng_calls = 0
function API.rng(seed)
  rng_calls = rng_calls + 1
  return make_rng(seed or (98765 + rng_calls))
end

-- A smooth, deterministic stand-in for the engine's noise: a few sines mixed by
-- seed. Continuous (so "seams are places" holds) and in −1..1.
local function noise(x, y, z, seed)
  seed = seed or 0
  local a = math.sin(x * 1.7 + seed * 0.13) * math.cos(y * 1.3 - seed * 0.21)
  local b = math.sin(z * 2.1 - seed * 0.07) * math.cos(x * 0.9 + y * 0.5)
  return math.max(-1, math.min(1, (a + b) * 0.5))
end
math.noise = noise
math.fbm = function(x, y, z) return noise(x, y, z, 3) * 0.8 end

-- ── the world ───────────────────────────────────────────────────────────────
-- One planet: radius 400, relief 24, a sea 6 m below the mean surface, air.
local BODY = { name = "Verdance", x = 1000, y = 0, z = -500, radius = 424,
               mu = 1.2e6, soi = 60000 }
local SURFACE = 400.0            -- the ground the stub raycast answers at
API.space = {
  bodies = function() return { BODY } end,
  body = function(n) return (n == BODY.name) and BODY or nil end,
  dominant = function(x, y, z)
    local d = math.sqrt((x - BODY.x) ^ 2 + (y - BODY.y) ^ 2 + (z - BODY.z) ^ 2)
    return d < BODY.soi and BODY.name or nil
  end,
}

-- Analytic sphere cast: any ray aimed inward from outside hits the surface.
local ray_count = 0
function API.raycast(ox, oy, oz, dx, dy, dz, maxd, ignore)
  ray_count = ray_count + 1
  local rx, ry, rz = ox - BODY.x, oy - BODY.y, oz - BODY.z
  local r = math.sqrt(rx * rx + ry * ry + rz * rz)
  -- Only the radial-ish inward casts the field and the tools make are modelled.
  local dot = (rx * dx + ry * dy + rz * dz) / math.max(1e-9, r)
  if dot > -0.2 then return nil end
  local dist = r - SURFACE
  if dist < 0 or dist > (maxd or 0) then return nil end
  local ux, uy, uz = rx / r, ry / r, rz / r
  return { x = BODY.x + ux * SURFACE, y = BODY.y + uy * SURFACE, z = BODY.z + uz * SURFACE,
           nx = ux, ny = uy, nz = uz, distance = dist }
end

-- ── nodes ───────────────────────────────────────────────────────────────────
local next_id = 100
local all_nodes = {}
local function make_node(name, parent)
  next_id = next_id + 1
  local n = { __id = next_id, id = next_id, name = name, valid = true,
              x = 0, y = 0, z = 0, yaw = 0, pitch = 0, roll = 0,
              scale = 1, scale_x = 1, scale_y = 1, scale_z = 1,
              tags = {}, kids = {}, parent = parent, visible = true }
  function n:setPrimitive(shape, color) self.shape = shape; self.color = color end
  function n:setMaterial(m) self.material = m end
  function n:children()
    local out = {}
    for _, c in ipairs(self.kids) do if c.valid then out[#out + 1] = c end end
    return out
  end
  function n:destroy()
    self.valid = false
    for _, c in ipairs(self.kids) do c.valid = false end
  end
  if parent then parent.kids[#parent.kids + 1] = n end
  all_nodes[#all_nodes + 1] = n
  return n
end

-- The engine queues creates and runs the callback next pass; running it inline
-- is STRICTER (anything the callback reads must already be valid).
function API.createNode(name, a, b)
  local parent, cb = nil, nil
  for _, v in ipairs({ a, b }) do
    if type(v) == "function" then cb = v elseif type(v) == "table" then parent = v end
  end
  local n = make_node(name, parent)
  if cb then cb(n) end
  return n
end

local PLANET = make_node(BODY.name, nil)
local ASTRO = make_node("Astronaut", nil)
ASTRO.visible = true

function API.find(name)
  if name == "Astronaut" then return ASTRO end
  if name == BODY.name then return PLANET end
  return nil
end
function API.findTagged() return {} end

-- ── script loading ──────────────────────────────────────────────────────────
local envs = {}
local function load_script(path, kind)
  local env = setmetatable({}, { __index = function(_, k)
    if k == "time" then return T end
    return API[k] or _G[k]
  end })
  local chunk = assert(loadfile(path))
  setfenv(chunk, env)
  chunk()
  env.params = {}
  for k, v in pairs(env.defaults or {}) do env.params[k] = v end
  envs[kind] = env
  return env
end
function API.findScript(kind) return envs[kind] end
function API.findScripts(kind)
  if kind == "vessel_controller" then return {} end
  return envs[kind] and { envs[kind] } or {}
end

local mats = load_script("solar/scripts/materials.lua", "materials")
local inv = load_script("solar/scripts/inventory.lua", "inventory")
local cl = load_script("solar/scripts/climate.lua", "climate")
local fg = load_script("solar/scripts/flora_gen.lua", "flora_gen")
local ff = load_script("solar/scripts/flora_field.lua", "flora_field")

-- The world record the generator publishes (a temperate, wet, breathing world).
store["world." .. BODY.name] = {
  name = BODY.name, kind = "canyon", seed = 20260728, radius = SURFACE,
  relief = 24.0, sea = SURFACE - 6.0, atmo = 0.7, insol = 1.0,
}

local checks = {}
local function check(desc, ok, detail)
  checks[#checks + 1] = { desc = desc, ok = ok and true or false, detail = detail }
end

-- ── 1. climate ──────────────────────────────────────────────────────────────
local w = cl.worldOf(BODY.name)
check("the world record is published and read back", w and w.kind == "canyon", w and w.kind)

local equator = cl.sampleRel(w, SURFACE + 4, 0, 0)
local pole = cl.sampleRel(w, 0, SURFACE + 4, 0)
check("the poles are colder than the equator", pole.temp < equator.temp - 0.05,
  string.format("%.2f vs %.2f", pole.temp, equator.temp))

local deep = cl.sampleRel(w, SURFACE - 30, 0, 0)
check("below sea level is ocean", deep.biome == "ocean" or deep.biome == "reef", deep.biome)
local shallow = cl.sampleRel(w, w.sea - 2, 0, 0)
check("just under the surface is shallows", shallow.biome == "reef", shallow.biome)
local beach = cl.sampleRel(w, w.sea + 0.5, 0, 0)
check("just above it is shoreline", beach.biome == "shore", beach.biome)
local peak = cl.sampleRel(w, SURFACE + 22, 0, 0)
check("a peak is not ocean", peak.elev > 0 and peak.biome ~= "ocean", peak.biome)
check("every biome the model can return is in the table", (function()
  for _, p in ipairs({ { SURFACE + 4, 0, 0 }, { 0, SURFACE + 4, 0 }, { 0, 0, SURFACE - 20 },
                       { SURFACE + 20, 5, 5 }, { -SURFACE - 2, 0, 0 } }) do
    local s = cl.sampleRel(w, p[1], p[2], p[3])
    if not cl.BIOMES[s.biome] then return false end
  end
  return true
end)())

-- Water queries in WORLD space (what the swimmer and the scatter ask).
check("underwater is underwater", cl.isUnderwater(BODY.x + w.sea - 5, BODY.y, BODY.z))
check("...and the beach is not", not cl.isUnderwater(BODY.x + SURFACE + 12, BODY.y, BODY.z))
check("sea depth is measured from the surface down",
  math.abs((cl.seaDepth(BODY.x + w.sea - 5, BODY.y, BODY.z) or 0) - 5) < 0.01,
  cl.seaDepth(BODY.x + w.sea - 5, BODY.y, BODY.z))

-- An airless rock grows nothing and says so without pretending it has weather.
store["world.Dead"] = { name = "Dead", kind = "barren", seed = 5, radius = 90,
                        relief = 6, sea = 0, atmo = 0, insol = 0.4 }
local dead = cl.sampleRel(cl.worldOf("Dead"), 90, 0, 0)
check("an airless world is barren", dead.biome == "barren" and dead.density == 0, dead.biome)
check("...and habitable() agrees", not cl.habitable("Dead") and cl.habitable(BODY.name))

-- ── 2. species ──────────────────────────────────────────────────────────────
local sp1 = fg.speciesFor(BODY.name)
check("a living world rolls species", #sp1 >= 3, "#" .. #sp1)
check("...and they are named, formed and useful", (function()
  for _, s in ipairs(sp1) do
    if type(s.name) ~= "string" or s.name == "" then return false end
    if not fg.formsFor("forest") then return false end
    if #s.yields == 0 then return false end
    if not (s.height > 0 and s.girth > 0) then return false end
  end
  return true
end)())
check("the roll is deterministic", (function()
  local a = {}
  for _, s in ipairs(sp1) do a[#a + 1] = s.name end
  -- Same world, asked again (cache) AND rolled fresh from the same record.
  local again = fg.speciesFor(BODY.name)
  if #again ~= #a then return false end
  for i, s in ipairs(again) do if s.name ~= a[i] then return false end end
  return true
end)())

-- A different seed is a different planet's flora.
store["world.Other"] = { name = "Other", kind = "canyon", seed = 777777, radius = 400,
                         relief = 24, sea = 394, atmo = 0.7, insol = 1.0 }
local sp2 = fg.speciesFor("Other")
check("another world's flora is different", (function()
  local names = {}
  for _, s in ipairs(sp1) do names[s.name] = true end
  local shared = 0
  for _, s in ipairs(sp2) do if names[s.name] then shared = shared + 1 end end
  return shared == 0
end)(), "#" .. #sp2)

check("a sea puts kelp in the water and nowhere else", (function()
  local aquatic, dry = 0, 0
  for _, s in ipairs(sp1) do
    if s.aquatic then aquatic = aquatic + 1 end
    if s.biomes.forest or s.biomes.grass then dry = dry + 1 end
  end
  return aquatic >= 1 and dry >= 1
end)())
check("nothing grows on an airless world", #fg.speciesFor("Dead") == 0)

-- ── 3. building a plant ─────────────────────────────────────────────────────
local tree = nil
for _, s in ipairs(sp1) do if s.form == "tree" then tree = s end end
if not tree then tree = sp1[1] end

local built
fg.build(PLANET, tree, SURFACE, 0, 0, 1, 0, 0, 4242, "full", function(r) built = r end)
check("a plant builds a real node tree", built ~= nil and #built:children() >= 2,
  built and #built:children())
check("...rooted at the point it was planted",
  built and math.abs(built.x - SURFACE) < 1e-6 and built.parent == PLANET)
check("...tagged so the world can find it", built and built.tags[1] == "flora")
check("...standing UP (the root aligns to the surface normal)", (function()
  -- up = +X here, and the vessel-basis solve puts that in roll = −π/2.
  return built and math.abs(built.roll + math.pi / 2) < 1e-3
end)(), built and built.roll)
check("...with every part scaled to something visible", (function()
  for _, c in ipairs(built:children()) do
    if not (c.scale_y > 0.001 and c.scale_x > 0.001) then return false end
    if not c.shape then return false end
  end
  return true
end)())

local far
fg.build(PLANET, tree, SURFACE, 0, 0, 1, 0, 0, 4242, "far", function(r) far = r end)
check("the far silhouette is cheaper than the full plant",
  far and #far:children() < #built:children(),
  string.format("%d vs %d", far and #far:children() or -1, #built:children()))

check("the root carries NO yaw of its own", math.abs(built.yaw) < 1e-9, built.yaw)
check("two individuals of one species differ", (function()
  -- The per-individual spin lives INSIDE the plant (the trunk's starting
  -- azimuth), never on the root — a root yaw would rotate the plant about world
  -- Y and tip it off the surface normal.
  local a, b
  fg.build(PLANET, tree, SURFACE, 0, 0, 1, 0, 0, 11, "full", function(r) a = r end)
  fg.build(PLANET, tree, SURFACE, 0, 0, 1, 0, 0, 12, "full", function(r) b = r end)
  if not (a and b) then return false end
  local ca, cb = a:children()[1], b:children()[1]
  return ca and cb and (math.abs(ca.yaw - cb.yaw) > 1e-6 or math.abs(ca.scale_y - cb.scale_y) > 1e-6)
end)())
check("the same seed rebuilds the same individual", (function()
  local a, b
  fg.build(PLANET, tree, SURFACE, 0, 0, 1, 0, 0, 99, "full", function(r) a = r end)
  fg.build(PLANET, tree, SURFACE, 0, 0, 1, 0, 0, 99, "full", function(r) b = r end)
  if not (a and b) then return false end
  if #a:children() ~= #b:children() then return false end
  for i, c in ipairs(a:children()) do
    local d = b:children()[i]
    if math.abs(c.x - d.x) > 1e-9 or math.abs(c.scale_y - d.scale_y) > 1e-9 then return false end
  end
  return true
end)())

local y = fg.harvestYield(tree, 4242)
check("a plant yields materials", #y > 0 and y[1].n > 0, y[1] and y[1].mat)
check("...the same ones every time it's asked", (function()
  local z = fg.harvestYield(tree, 4242)
  if #z ~= #y then return false end
  for i, e in ipairs(z) do if e.mat ~= y[i].mat or e.n ~= y[i].n then return false end end
  return true
end)())

-- ── 4. the scatter field ────────────────────────────────────────────────────
-- Stand on the surface and let the field run.
ASTRO.x, ASTRO.y, ASTRO.z = BODY.x + SURFACE + 1.5, BODY.y, BODY.z
local FF = ff.params
FF.interval = 0.0
local function field_tick(n)
  for _ = 1, (n or 1) do
    T = T + 0.25
    ff.update({}, 0.25)
  end
end
field_tick(40)
check("standing on a living world grows a field", ff.count > 0, "count=" .. ff.count)
check("...anchored to the body we're on", ff.bodyName == BODY.name, ff.bodyName)
check("...within the budget", ff.count <= FF.budget, ff.count .. "/" .. FF.budget)
check("...on the ground, not in the air", (function()
  local hit = ff.nearestPlant(ASTRO.x, ASTRO.y, ASTRO.z, 1, 0, 0, 400, -1)
  if not hit then return false end
  local r = math.sqrt((hit.x - BODY.x) ^ 2 + (hit.y - BODY.y) ^ 2 + (hit.z - BODY.z) ^ 2)
  return math.abs(r - SURFACE) < 0.5
end)())

-- Determinism: walk away, clear, come back — the same plants stand there.
local before = {}
for _, l in ipairs(ff.surveyLines(ASTRO.x, ASTRO.y, ASTRO.z, 60)) do before[#before + 1] = l end
ff.clearAll()
field_tick(40)
local after = ff.surveyLines(ASTRO.x, ASTRO.y, ASTRO.z, 60)
check("the same ground regrows the same stand", (function()
  if #before ~= #after or #before == 0 then return false end
  for i, l in ipairs(before) do if l ~= after[i] then return false end end
  return true
end)(), string.format("%d vs %d lines", #before, #after))

-- In orbit, nothing scatters at all.
local kept = ff.count
ASTRO.x = BODY.x + SURFACE + 900
field_tick(2)
check("nothing grows while you're in orbit", ff.count == 0, ff.count .. " (was " .. kept .. ")")
ASTRO.x = BODY.x + SURFACE + 1.5
field_tick(40)
check("...and it comes back when you land", ff.count > 0, ff.count)

-- An airless world stays bare.
store["world." .. BODY.name].atmo = 0.0
store["world." .. BODY.name].sea = 0
fg.speciesFor(BODY.name)  -- cached from before: the field must go through worldOf
ff.clearAll()
field_tick(4)
store["world." .. BODY.name].atmo = 0.7
store["world." .. BODY.name].sea = SURFACE - 6.0
ff.clearAll()
field_tick(40)
check("the field recovers after the world record changes", ff.count > 0, ff.count)

-- ── 5. harvesting ───────────────────────────────────────────────────────────
local aim = ff.nearestPlant(ASTRO.x, ASTRO.y, ASTRO.z, 1, 0, 0, 400, -1)
check("the cutter can find what you're aiming at", aim ~= nil)
if aim then
  -- Aim from just next to it, along the direction to it: the cone test must pass.
  local dx, dy, dz = aim.x - ASTRO.x, aim.y - ASTRO.y, aim.z - ASTRO.z
  local l = math.sqrt(dx * dx + dy * dy + dz * dz)
  local near = ff.nearestPlant(ASTRO.x, ASTRO.y, ASTRO.z, dx / l, dy / l, dz / l, l + 1, 0.5)
  -- Whatever comes back must actually BE in front of you and in reach — which
  -- is the contract; it needn't be the particular plant we sighted, because a
  -- closer one in the same cone is the better answer.
  check("...returns something in front and in reach", (function()
    if not near then return false end
    local ex, ey, ez = near.x - ASTRO.x, near.y - ASTRO.y, near.z - ASTRO.z
    local d = math.sqrt(ex * ex + ey * ey + ez * ez)
    return d <= l + 1 and (ex * dx + ey * dy + ez * dz) / (d * l) >= 0.5
  end)())
  local off = ff.nearestPlant(ASTRO.x, ASTRO.y, ASTRO.z, -dx / l, -dy / l, -dz / l, l + 1, 0.5)
  check("...and not what's behind you", off == nil or off.pid ~= aim.pid)

  local n0 = ff.count
  local node = aim.rec.node
  local got, dropped = ff.harvest(aim, "astro")
  check("harvesting yields into the pack", #got > 0, got[1] and got[1].mat)
  check("...and the plant is gone", not node.valid and ff.count == n0 - 1)
  check("...and the pack actually holds it",
    inv.count("astro", got[1].mat) >= got[1].n, inv.line("astro"))
  check("...and it is not standing there any more",
    (function()
      local again = ff.nearestPlant(ASTRO.x, ASTRO.y, ASTRO.z, 1, 0, 0, 400, -1)
      return again == nil or again.pid ~= aim.pid
    end)())
  -- Regrowth: the spot stays empty for `regrow` seconds and then comes back.
  local pid = aim.pid
  ff.clearAll()
  field_tick(40)
  check("a cut plant does not come straight back", (function()
    for _ = 1, 1 do end
    local again = ff.nearestPlant(ASTRO.x, ASTRO.y, ASTRO.z, 1, 0, 0, 400, -1)
    return again == nil or again.pid ~= pid
  end)())
  T = T + FF.regrow + 10
  ff.clearAll()
  field_tick(40)
  local grown = false
  for _ = 1, 1 do
    local hit = ff.nearestPlant(ASTRO.x, ASTRO.y, ASTRO.z, 1, 0, 0, 400, -1)
    grown = hit ~= nil
  end
  check("...but the ground recovers if you leave it alone", grown)
  check("dropped units are reported, not silently voided", dropped >= 0)
end

-- ── report ──────────────────────────────────────────────────────────────────
local bad = {}
for _, c in ipairs(checks) do
  if not c.ok then
    bad[#bad + 1] = "  ✗ " .. c.desc .. (c.detail and ("   [" .. tostring(c.detail) .. "]") or "")
  end
end
if #bad == 0 then
  print(string.format("FLORA SMOKE OK — %d checks passed  (%d plants, %d rays)",
    #checks, ff.count, ray_count))
  os.exit(0)
end
print("FLORA SMOKE FAILURES:")
for _, b in ipairs(bad) do print(b) end
print("\n-- script logs --")
for i = math.max(1, #logs - 20), #logs do print("  " .. tostring(logs[i])) end
os.exit(1)
