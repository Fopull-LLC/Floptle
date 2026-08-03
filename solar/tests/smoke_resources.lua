-- SMOKE HARNESS: the material economy. Runs the real materials.lua,
-- inventory.lua, tool_belt.lua, research.lua and the depot half of
-- facility_menu.lua against a stubbed engine, and drives the whole loop the
-- game is built on:
--
--   mine rock → the pack fills (and refuses to overfill) → carry it to a craft
--   → the hold takes what fits → land at base → the crane unloads → the depot
--   sells → the discovery pays → R&D opens the next part.
--
--   luajit solar/tests/smoke_resources.lua
--
-- Fidelity notes (the bugs this stub is shaped to catch):
--  * MASS is the whole limit, so every container here is small enough to fill.
--    A transfer that "succeeds" by exceeding capacity, or one that loses the
--    units that didn't fit, fails a check rather than quietly costing a player
--    an afternoon's mining.
--  * The world record is what mining reads (archetype + seed), so a tool that
--    forgets to resolve the body yields the wrong planet's ore.
--  * Everything is `save.*`-backed: the store here persists across "scene
--    loads" exactly like the engine's.

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
function API.spawnEffect() end
API.draw = { line = function() end }

local function noise(x, y, z, seed)
  seed = seed or 0
  local a = math.sin(x * 1.7 + seed * 0.13) * math.cos(y * 1.3 - seed * 0.21)
  local b = math.sin(z * 2.1 - seed * 0.07) * math.cos(x * 0.9 + y * 0.5)
  return math.max(-1, math.min(1, (a + b) * 0.5))
end
math.noise = noise

local BODY = { name = "Golil", x = 0, y = 0, z = 0, radius = 424, mu = 3.5e6, soi = 1e6 }
local SURFACE = 400.0
API.space = {
  bodies = function() return { BODY } end,
  body = function(n) return (n == BODY.name) and BODY or nil end,
  dominant = function() return BODY.name end,
}
store["world." .. BODY.name] = {
  name = BODY.name, kind = "canyon", seed = 4242, radius = SURFACE,
  relief = 24.0, sea = 0, atmo = 0.6, insol = 1.0,
}

-- The dig target: a point `dig_depth` metres under the surface, straight ahead.
local dig_depth = 0.0
local digs = 0
API.terrain = {
  dig = function() digs = digs + 1 end,
  sculpt = function() digs = digs + 1 end,
}
function API.raycast(ox, oy, oz, dx, dy, dz, maxd)
  -- Two casts happen per dab: the tool's AIM (from the camera, +X) and the
  -- DEPTH PROBE (from high above the hit point, straight back down its radial).
  local r = SURFACE - dig_depth
  if dx > 0.5 then
    -- The face wanders as you cut, the way a real shaft does — which is what
    -- makes the ore-vein noise field mean anything.
    return { x = r, y = (digs % 13) * 3.3, z = (digs % 11) * 4.1,
             nx = 1, ny = 0, nz = 0, distance = 4.0 }
  end
  -- The probe: answer on the surface sphere along the origin's own radial, so a
  -- shaft's tenth metre really is ten metres down.
  local l = math.sqrt(ox * ox + oy * oy + oz * oz)
  if l < 1e-6 then return nil end
  local ux, uy, uz = ox / l, oy / l, oz / l
  return { x = ux * SURFACE, y = uy * SURFACE, z = uz * SURFACE,
           nx = ux, ny = uy, nz = uz, distance = l - SURFACE }
end

local keys = {}
API.input = {
  key = function(k) return keys[k] == true end,
  pressed = function(k) return keys["!" .. k] == true end,
  button = function() return false end,
}
local function press(k) keys["!" .. k] = true end
local function clear_presses()
  for k in pairs(keys) do if k:sub(1, 1) == "!" then keys[k] = nil end end
end

local ASTRO = { __id = 1, id = 1, name = "Astronaut", valid = true, visible = true,
                x = SURFACE + 2, y = 0, z = 0 }
local DEPOT = { __id = 2, id = 2, name = "FacDepot", valid = true, x = SURFACE + 10, y = 0, z = 0 }
local SHIP = { __id = 3, id = 3, name = "Kestrel I", valid = true,
               x = SURFACE + 6, y = 0, z = 0 }
local FAR_SHIP = { __id = 4, id = 4, name = "Stranded", valid = true,
                   x = SURFACE + 900, y = 0, z = 0 }

function API.find(name)
  if name == "Astronaut" then return ASTRO end
  if name == "FacDepot" then return DEPOT end
  return nil
end
function API.createNode() end

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
  if kind == "vessel_controller" then return envs.__vessels or {} end
  return envs[kind] and { envs[kind] } or {}
end

local mats = load_script("solar/scripts/materials.lua", "materials")
local inv = load_script("solar/scripts/inventory.lua", "inventory")
local co = load_script("solar/scripts/company.lua", "company")
local cl = load_script("solar/scripts/climate.lua", "climate")
local rs = load_script("solar/scripts/research.lua", "research")
local fm = load_script("solar/scripts/facility_menu.lua", "facility_menu")
local tb = load_script("solar/scripts/tool_belt.lua", "tool_belt")

-- The camera the tools aim along: straight out +X from just behind the crew.
envs.planet_camera = { cam_x = SURFACE + 6, cam_y = 0, cam_z = 0,
                       fwd_x = 1, fwd_y = 0, fwd_z = 0 }

-- Two "vessels": one parked at the base with a hold, one stranded far away.
-- Only the interface the depot and the panel use is stubbed.
local function fake_vessel(node, holdcap)
  local v = { node = node, craftName = node.name,
              holdId = holdcap > 0 and ("hold:" .. node.name) or nil,
              holdCap = holdcap, applied = 0 }
  function v.holdApplyMass() v.applied = v.applied + 1 end
  if v.holdId then inv.setCap(v.holdId, holdcap) end
  return v
end
local KESTREL = fake_vessel(SHIP, 60.0)
local STRANDED = fake_vessel(FAR_SHIP, 200.0)
envs.__vessels = { KESTREL, STRANDED }

co.start({})
tb.start(ASTRO)

local checks = {}
local function check(desc, ok, detail)
  checks[#checks + 1] = { desc = desc, ok = ok and true or false, detail = detail }
end

-- ── 1. the registry ─────────────────────────────────────────────────────────
check("every material is weighed and priced", (function()
  for _, id in ipairs(mats.list()) do
    local d = mats.byId(id)
    if not (d and d.kg > 0 and d.value >= 0 and d.name) then return false end
  end
  return #mats.list() > 8
end)())
check("the listing order is stable", (function()
  local a, b = mats.list(), mats.list()
  for i, id in ipairs(a) do if b[i] ~= id then return false end end
  return true
end)())

check("a surface dig never turns up deep ore", (function()
  for i = 0, 200 do
    local m = mats.pickOre("canyon", 0.0, i / 200)
    if m == "uranium" or m == "crystal" then return false end
    if not mats.byId(m) then return false end
  end
  return true
end)())
check("a deep shaft can", (function()
  local deep = false
  for i = 0, 400 do
    local m = mats.pickOre("canyon", 40.0, i / 400)
    if m == "uranium" or m == "crystal" then deep = true end
  end
  return deep
end)())
check("the same hole always yields the same thing",
  mats.pickOre("canyon", 12, 0.42) == mats.pickOre("canyon", 12, 0.42))
check("an unknown archetype still yields something",
  mats.byId(mats.pickOre("no-such-world", 5, 0.5)) ~= nil)
check("an ice world yields ice", (function()
  local ice = false
  for i = 0, 100 do if mats.pickOre("ice", 1, i / 100) == "ice" then ice = true end end
  return ice
end)())

-- ── 2. containers ───────────────────────────────────────────────────────────
check("a fresh pack is empty", inv.isEmpty("astro") and inv.mass("astro") == 0)
check("the suit's capacity is what the params say", inv.cap("astro") == 45.0, inv.cap("astro"))

-- Iron is 2.6 kg: 17 units fit in 45 kg, the 18th does not.
local took = inv.add("astro", "iron", 17)
check("a pack takes what fits", took == 17, took)
local over = inv.add("astro", "iron", 5)
check("...and refuses what doesn't, partially and honestly", over == 0,
  string.format("took %d more, %.1f kg", over, inv.mass("astro")))
check("...without ever exceeding capacity", inv.mass("astro") <= inv.cap("astro") + 1e-9,
  inv.line("astro"))
local part = inv.add("astro", "fiber", 20)   -- 0.3 kg each; 1.8 kg of room left
check("a partial add reports what actually went in", part > 0 and part < 20, part)

check("discoveries are recorded as things are held", inv.seen("iron") and not inv.seen("uranium"))
check("the pack values itself", inv.totalValue("astro") > 0, inv.totalValue("astro"))
check("contents list in registry order", (function()
  local it = inv.items("astro")
  if #it < 2 then return false end
  return mats.tier(it[1].mat) <= mats.tier(it[#it].mat)
end)())

-- Transfers: what fits moves, the rest stays put. Nothing is ever destroyed.
local before_units = 0
for _, it in ipairs(inv.items("astro")) do before_units = before_units + it.n end
inv.setCap("hold:Kestrel I", 10.0)          -- deliberately tiny
local moved = inv.transferAll("astro", "hold:Kestrel I")
local after_units = 0
for _, it in ipairs(inv.items("astro")) do after_units = after_units + it.n end
for _, it in ipairs(inv.items("hold:Kestrel I")) do after_units = after_units + it.n end
check("a transfer moves only what fits", moved > 0 and moved < before_units, moved)
check("...and loses nothing on the way", after_units == before_units,
  string.format("%d → %d", before_units, after_units))
check("...and the hold respects its own capacity",
  inv.mass("hold:Kestrel I") <= 10.0 + 1e-9, inv.line("hold:Kestrel I"))
inv.setCap("hold:Kestrel I", 60.0)

-- ── 3. the tools ────────────────────────────────────────────────────────────
inv.clear("astro")
inv.clear("hold:Kestrel I")

local function tick(n)
  for _ = 1, (n or 1) do
    T = T + 0.05
    tb.update(ASTRO, 0.05)
    clear_presses()
  end
end

check("the belt starts on the mining laser", tb.toolId() == "laser", tb.toolId())
keys.e = true
dig_depth = 1.0
tick(60)
keys.e = false
check("holding the laser cuts rock", digs > 0, digs)
check("...and the pack fills with it", not inv.isEmpty("astro"), inv.line("astro"))
check("...with this world's ore", (function()
  for _, it in ipairs(inv.items("astro")) do
    local ok = false
    for _, m in ipairs(mats.oresOf("canyon")) do if m == it.mat then ok = true end end
    if not ok then return false end
  end
  return true
end)(), inv.items("astro")[1] and inv.items("astro")[1].mat)

-- Deep rock is different rock: same tool, same spot, 40 m down.
local shallow_kinds = {}
for _, it in ipairs(inv.items("astro")) do shallow_kinds[it.mat] = true end
inv.clear("astro")
dig_depth = 40.0
keys.e = true
tick(120)
keys.e = false
check("a deep shaft yields deep materials", (function()
  for _, it in ipairs(inv.items("astro")) do
    if mats.tier(it.mat) >= 2 then return true end
  end
  return false
end)(), (function()
  local s = {}
  for _, it in ipairs(inv.items("astro")) do s[#s + 1] = it.mat end
  return table.concat(s, ",")
end)())

-- A full pack stops taking, and the HUD says why rather than silently voiding.
local fillers = math.ceil(inv.cap("astro") / mats.kg("regolith"))
inv.add("astro", "regolith", fillers)
keys.e = true
tick(20)
keys.e = false
check("a full pack refuses more and says so",
  inv.mass("astro") <= inv.cap("astro") + 1e-9 and tb.hudLine():find("full") ~= nil,
  tb.hudLine())

-- The spade shapes ground and yields NOTHING (free digging died with SC4).
inv.clear("astro")
press("3")
tick(1)
check("the belt switches tools on the number keys", tb.toolId() == "spade", tb.toolId())
local d0 = digs
keys.e = true
tick(30)
keys.e = false
check("the spade still moves ground", digs > d0)
check("...but pays nothing for it", inv.isEmpty("astro"), inv.line("astro"))
press("1")
tick(1)
check("...and switching back is one key", tb.toolId() == "laser")

-- ── 4. the depot ────────────────────────────────────────────────────────────
inv.clear("astro")
inv.clear("base")
inv.add("astro", "iron", 6)
inv.add("hold:Kestrel I", "timber", 8)
inv.add("hold:Stranded", "crystal", 4)

local holds = fm.landedHolds()
check("the crane sees the craft parked at the base", #holds == 1 and holds[1].id == "hold:Kestrel I",
  #holds .. " hold(s)")
check("...and not the one stranded across the planet", (function()
  for _, h in ipairs(holds) do if h.id == "hold:Stranded" then return false end end
  return true
end)())

local msg = fm.depotUnload()
check("unloading empties the hold and the pack into the warehouse",
  inv.isEmpty("hold:Kestrel I") and inv.isEmpty("astro") and not inv.isEmpty("base"), msg)
check("...and the stranded craft keeps its cargo", inv.count("hold:Stranded", "crystal") == 4)

local bal0 = co.balance()
local rep0 = co.rep()
local worth = inv.totalValue("base")
msg = fm.depotSell()
check("selling empties the warehouse", inv.isEmpty("base"), msg)
check("...and pays for it", co.balance() > bal0 + worth,   -- price + discovery premium
  string.format("%d → %d (goods %d)", bal0, co.balance(), worth))
check("...and a first sale is a discovery", co.rep() > rep0, string.format("%d → %d", rep0, co.rep()))

-- Selling the same material again is just money: the premium is paid once.
inv.add("base", "iron", 4)
local bal1, rep1 = co.balance(), co.rep()
fm.depotSell()
local plain = co.balance() - bal1
check("a repeat sale pays the goods and nothing extra",
  plain > 0 and plain < 150 and co.rep() == rep1, plain)

-- Standing moves the price.
co.addRep(5, "test")
inv.add("base", "iron", 10)
local bal2 = co.balance()
local list = mats.valueOf("iron", 10)
fm.depotSell()
check("reputation pays better", (co.balance() - bal2) > list,
  string.format("%d for a list price of %d", co.balance() - bal2, list))

check("an empty warehouse says so rather than paying zero",
  fm.depotSell():find("empty") ~= nil)

-- ── 5. research ─────────────────────────────────────────────────────────────
check("the starter parts need no research",
  rs.isUnlocked("pod") and rs.isUnlocked("tankS") and rs.isUnlocked("engineS"))
check("the rest are locked", not rs.isUnlocked("tankM") and not rs.isUnlocked("dockPort"))

-- A material lock names the sample it wants, and holding one clears it. Mining
-- has already turned copper up by now, so forget it for a moment: the discovery
-- set is the real gate and this is the real path through it.
local seen_set = store["inv.seen"]
seen_set.copper = nil
check("a part gated on a material says which", (function()
  local why = rs.blockedBy("battery")
  return why ~= nil and why:find("Copper") ~= nil
end)(), rs.blockedBy("battery"))
inv.add("astro", "copper", 1)
check("...and finding some clears that gate", rs.blockedBy("battery") == nil,
  rs.blockedBy("battery"))

local bal3 = co.balance()
local ok, why = rs.unlock("battery")
check("researching it works", ok and rs.isUnlocked("battery"), why)
check("...and is paid for", co.balance() == bal3 - rs.costOf("battery"),
  string.format("%d → %d", bal3, co.balance()))
check("...and re-researching is free and harmless", (function()
  local b = co.balance()
  local ok2 = rs.unlock("battery")
  return ok2 and co.balance() == b
end)())

check("a laddered part waits for its prerequisite", (function()
  -- The docking port sits behind RCS, which sits behind the Anvil: the lock
  -- names the NEXT rung, not the whole chain.
  local w = rs.blockedBy("dockPort")
  return w ~= nil and w:find("RCS") ~= nil
end)(), rs.blockedBy("dockPort"))
check("the lock line reads as an instruction", (function()
  local l = rs.lockLine("skipper")
  return l ~= nil and l:find("LOCKED") == 1
end)(), rs.lockLine("skipper"))

-- No money = no research, and nothing half-happens.
co.spend(co.balance(), "test: broke")
inv.add("astro", "iron", 1)
rs.unlock("engineM")           -- clear the ladder for skipper
local ok2, why2 = rs.unlock("skipper")
check("a company that can't pay doesn't get the tech",
  not ok2 and not rs.isUnlocked("skipper"), why2)
check("...and the balance is untouched", co.balance() == 0, co.balance())

check("the pending list explains everything still locked", (function()
  local lines = rs.pendingLines()
  if #lines == 0 then return false end
  for _, l in ipairs(lines) do if not l:find("LOCKED") then return false end end
  return true
end)(), #rs.pendingLines() .. " locked")

-- ── report ──────────────────────────────────────────────────────────────────
local bad = {}
for _, c in ipairs(checks) do
  if not c.ok then
    bad[#bad + 1] = "  ✗ " .. c.desc .. (c.detail and ("   [" .. tostring(c.detail) .. "]") or "")
  end
end
if #bad == 0 then
  print(string.format("RESOURCE SMOKE OK — %d checks passed", #checks))
  os.exit(0)
end
print("RESOURCE SMOKE FAILURES:")
for _, b in ipairs(bad) do print(b) end
print("\n-- script logs --")
for i = math.max(1, #logs - 24), #logs do print("  " .. tostring(logs[i])) end
os.exit(1)
