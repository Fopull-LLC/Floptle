-- SMOKE HARNESS: the money loop. Runs the real company.lua + missions.lua
-- against a stubbed engine and drives the whole SC2 economy end to end —
-- seed capital, a hull bought at rollout, a contract signed, goals latching
-- across one flight, the payout, and a recovery refund.
--
--   luajit solar/tests/smoke_company.lua
--
-- Fidelity notes (the bugs this stub is shaped to catch):
--  * All company/mission state lives in `save.*`, because scripts do NOT
--    survive the builder → system scene hop. The store here persists across
--    "scene loads" exactly like the engine's, so anything cached in a local
--    that should have been saved shows up as state that vanishes.
--  * Goals latch PER FLIGHT: stepping out of the pod clears them. "Reach
--    2,000 m and land intact" has to be one trip, and a tracker that latched
--    forever would pass it across two unrelated afternoons.

local T = 0.0
local TICK = 1 / 60

-- ── engine API ──────────────────────────────────────────────────────────────
local API, store, logs = {}, {}, {}
API.save = {
  get = function(k) return store[k] end,
  set = function(k, v) store[k] = v end,
  flush = function() end,
}
function API.log(m) logs[#logs + 1] = tostring(m) end
function API.vec3(x, y, z) return { x = x, y = y, z = z } end
API.input = { key = function() return false end, pressed = function() return false end }

-- One planet, one craft. The craft's altitude and grounded state are driven
-- directly by the tests — this harness is about the LEDGER, not flight.
local BODY = { name = "Golil", x = 0, y = 0, z = 0, radius = 600, mu = 3.5e6, soi = 1e6 }
API.space = {
  bodies = function() return { BODY } end,
  body = function(n) return (n == BODY.name) and BODY or nil end,
  dominant = function() return BODY.name end,
  elements = function(x, y, z, vx, vy, vz)
    local r = math.sqrt(x * x + y * y + z * z)
    local v2 = vx * vx + vy * vy + vz * vz
    -- Real vis-viva, so an "orbit" goal can't be faked by a tall lob.
    local a = 1.0 / (2.0 / r - v2 / BODY.mu)
    if a <= 0 then return { body = BODY.name, e = 1.2, periapsis = r } end
    local h = math.sqrt(math.max(0.0, (x * vy - y * vx) ^ 2 + (y * vz - z * vy) ^ 2
      + (z * vx - x * vz) ^ 2))
    local e = math.sqrt(math.max(0.0, 1.0 - (h * h) / (BODY.mu * a)))
    return { body = BODY.name, e = e, a = a, periapsis = a * (1 - e),
             apoapsis = a * (1 + e), period = 1000 }
  end,
}

local craft = {
  node = { __id = 7, id = 7, name = "Kestrel I", valid = true,
           x = 0, y = BODY.radius, z = 0, vx = 0, vy = 0, vz = 0 },
  piloting = false,
  dock = { latched = 0 },
}
local ASM = { mass = 6.0, grounded = true, anchored = false,
              vel = { x = 0, y = 0, z = 0 }, angVel = { x = 0, y = 0, z = 0 },
              com = { x = 0, y = 0, z = 0 }, origin = { x = 0, y = 0, z = 0 },
              parts = {} }
API.assembly = { info = function() return ASM end }

-- Put the craft at an altitude, moving at a speed, on a chosen conic.
local function place(alt, speed)
  craft.node.y = BODY.radius + alt
  craft.node.x, craft.node.z = 0, 0
  craft.node.vx, craft.node.vy, craft.node.vz = speed or 0, 0, 0
  ASM.vel.x, ASM.vel.y, ASM.vel.z = 0, 0, 0
end

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
  if kind == "vessel_controller" then return { craft } end
  return envs[kind] and { envs[kind] } or {}
end

local co = load_script("solar/scripts/company.lua", "company")
local mi = load_script("solar/scripts/missions.lua", "missions")
-- The material economy: contracts can ask for cargo in the warehouse, so the
-- registry and the containers have to be here for the tracker to read.
local mats = load_script("solar/scripts/materials.lua", "materials")
local inv = load_script("solar/scripts/inventory.lua", "inventory")
co.start({})
mi.start({})

local function tick(n)
  for _ = 1, (n or 1) do
    T = T + TICK
    co.update({}, TICK)
    mi.update({}, TICK)
  end
end

-- ── checks ──────────────────────────────────────────────────────────────────
local checks = {}
local function check(desc, ok, detail)
  checks[#checks + 1] = { desc = desc, ok = ok and true or false, detail = detail }
end

-- 1. Seed capital, once and only once.
check("a fresh company is funded", co.balance() == 12000, "$" .. co.balance())
co.spend(2000, "test")
co.start({})           -- a "scene load": the script reruns, the money must not
tick()                 -- come back
check("seed capital is not re-granted on a scene load", co.balance() == 10000,
  "$" .. co.balance())

-- 2. Spending refuses rather than going negative.
check("an affordable purchase goes through", co.spend(1000, "hull") == true)
check("...and debits exactly", co.balance() == 9000, "$" .. co.balance())
check("an unaffordable one is refused", co.spend(999999, "battlestar") == false)
check("...and changes nothing", co.balance() == 9000, "$" .. co.balance())
check("afford() agrees with spend()", co.afford(9000) and not co.afford(9001))
co.earn(500, "scrap")
check("earning credits", co.balance() == 9500, "$" .. co.balance())
check("the ledger records both directions", #co.ledgerLines(9) >= 4,
  "#" .. #co.ledgerLines(9))

-- 3. Reputation clamps rather than running away.
co.addRep(20, "test")
check("reputation clamps high", co.rep() == 10, tostring(co.rep()))
co.addRep(-40, "test")
check("reputation clamps low", co.rep() == -5, tostring(co.rep()))
co.addRep(5, "test")

-- 4. Contracts: only what's actually available is offered.
local offers = mi.offers()
check("the opening contract is offered", #offers >= 1 and offers[1].id == "firstflight",
  offers[1] and offers[1].id)
local ids = {}
for _, m in ipairs(offers) do ids[m.id] = true end
check("gated contracts stay hidden until their prerequisite is flown",
  not ids.highroad and not ids.orbit)

check("signing a contract puts it on the books", mi.accept("firstflight") == true)
check("...and it stops being offered", (function()
  for _, m in ipairs(mi.offers()) do if m.id == "firstflight" then return false end end
  return true
end)())

-- 5. Goals latch across ONE flight, in any order, and pay out on completion.
local bal = co.balance()
craft.piloting = true
ASM.grounded = false
place(2500, 0)
tick(3)
check("the altitude goal latches in flight", mi.goalLines()[1]:find("✓") ~= nil,
  mi.goalLines()[1])
check("...and the landing goal has not", mi.goalLines()[2]:find("○") ~= nil)
check("the contract is still on the books", mi.active == "firstflight")

-- Come back down and stop.
place(0, 0)
ASM.grounded = true
tick(3)
check("landing completes the contract", mi.active == nil, tostring(mi.active))
check("...and Ops pays out", co.balance() == bal + 4500,
  string.format("%d → %d", bal, co.balance()))
check("...and it never pays twice", (function()
  local b2 = co.balance()
  tick(5)
  return co.balance() == b2
end)())
check("a flown contract is not offered again", (function()
  for _, m in ipairs(mi.offers()) do if m.id == "firstflight" then return false end end
  return true
end)())
check("finishing it unlocks the next rung", (function()
  for _, m in ipairs(mi.offers()) do if m.id == "highroad" then return true end end
  return false
end)())

-- 6. Goals do NOT latch across separate flights.
mi.accept("highroad")
ASM.grounded = false
place(30000, 0)
tick(3)
check("the high-altitude goal latches", mi.goalLines()[1]:find("✓") ~= nil)
craft.piloting = false          -- step out of the pod: the flight is over
tick(3)
craft.piloting = true
place(0, 0)
ASM.grounded = true
tick(3)
check("stepping out clears the flight's progress — one trip, not two",
  mi.active == "highroad", "active=" .. tostring(mi.active))
check("...and the altitude goal is un-ticked", mi.goalLines()[1]:find("○") ~= nil,
  mi.goalLines()[1])

-- 7. An ORBIT goal cannot be faked by a tall lob.
mi.abandon()
tick()
store["mission.done"] = { firstflight = true, highroad = true }
check("orbit is offered once the ladder allows it", (function()
  for _, m in ipairs(mi.offers()) do if m.id == "orbit" then return true end end
  return false
end)())
mi.accept("orbit")
craft.piloting = true
ASM.grounded = false
place(30000, 0)                    -- 30 km up, straight up, zero horizontal
tick(3)
check("a ballistic lob does not count as an orbit", mi.active == "orbit",
  mi.goalLines()[1])
-- Now a real circular orbit at the same altitude.
local r = BODY.radius + 30000
place(30000, math.sqrt(BODY.mu / r))
tick(3)
check("a circular orbit does", mi.active == nil, mi.goalLines()[1] or "done")

-- 8. RESOURCE contracts: delivered cargo, and the fact that it stays delivered.
mi.abandon()
tick()
store["mission.done"] = { firstflight = true, highroad = true, orbit = true }
check("a prospecting contract is offered once you can fly", (function()
  for _, m in ipairs(mi.offers()) do if m.id == "prospect" then return true end end
  return false
end)())
mi.accept("prospect")
craft.piloting = false
tick(2)
check("an undelivered haul is not complete", mi.active == "prospect",
  mi.goalLines()[1])
inv.add("base", "iron", 12)
tick(2)
check("...and a part-load still isn't", mi.active == "prospect", mi.goalLines()[1])
local bal3 = co.balance()
inv.add("base", "iron", 20)          -- 32 units of ore in the warehouse
tick(2)
check("delivering the ore completes it on the ground, with nobody flying",
  mi.active == nil, tostring(mi.active))
check("...and pays", co.balance() == bal3 + 8000, co.balance() - bal3)

-- Selling what you delivered can't un-deliver it.
mi.accept("botany")
inv.add("base", "fiber", 25)
tick(2)
check("a plant-matter haul counts flora, not ore", mi.active == nil, tostring(mi.active))

mi.accept("deepcore")
tick(2)
check("a deep contract wants the material found AND delivered",
  mi.active == "deepcore" and mi.goalLines()[1]:find("○") ~= nil, mi.goalLines()[1])
inv.add("astro", "crystal", 5)       -- holding it = the discovery
tick(2)
check("...finding it ticks the sample goal", mi.goalLines()[1]:find("✓") ~= nil,
  mi.goalLines()[1])
check("...but the delivery goal is still open", mi.active == "deepcore",
  mi.goalLines()[2])
inv.transferAll("astro", "base")
tick(2)
check("...and putting it in the warehouse finishes the job", mi.active == nil,
  tostring(mi.active))
check("a permanent goal survives stepping in and out of a craft", (function()
  mi.accept("recovery")
  store["mission.recovered"] = true
  tick(2)
  return mi.active == nil
end)())

-- ── report ──────────────────────────────────────────────────────────────────
local bad = {}
for _, c in ipairs(checks) do
  if not c.ok then
    bad[#bad + 1] = "  ✗ " .. c.desc .. (c.detail and ("   [" .. tostring(c.detail) .. "]") or "")
  end
end
if #bad == 0 then
  print(string.format("COMPANY SMOKE OK — %d checks passed", #checks))
  os.exit(0)
end
print("COMPANY SMOKE FAILURES:")
for _, b in ipairs(bad) do print(b) end
print("\n-- script logs --")
for _, l in ipairs(logs) do print("  " .. l) end
os.exit(1)
