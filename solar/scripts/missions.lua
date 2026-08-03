-- MISSIONS — the contracts that turn a sandbox into a company.
--
-- One accepted contract at a time. Its goals are watched live against the craft
-- you're actually flying; every goal that latches stays latched for the flight,
-- so "reach 2,000 m AND land intact" is a thing you do in one trip rather than
-- two states that can never be true at once. Complete them all and Ops pays out
-- the moment you're home.
--
-- The CATALOGUE below is the data. Goals are declarative and the tracker knows
-- a handful of kinds, so a new contract is a table entry — no new code:
--
--   altitude  value      climb this high above the body you launched from
--   speed     value      hit this surface speed (m/s)
--   orbit     value      periapsis above this altitude — a real orbit, not a lob
--   land      body?      touch down intact (optionally on a NAMED body)
--   dock                 latch onto another craft in flight
--   recover              bring the craft home and recover it at the base
--   deliver   mat/class  have this much of a material in the base WAREHOUSE
--                        (mined or harvested, flown home, unloaded at the depot)
--   sample    mat        have HELD this material at all — the discovery set
--
-- `deliver`, `sample` and `recover` latch PERMANENTLY (they're things you did,
-- not states of a flight); every other kind latches for the current flight
-- only, so a multi-goal trip has to be one trip.
--
-- Cross-script:  local m = findScript("missions")
--                m.offers()  m.accept(id)  m.abandon()  m.activeLine()

local CATALOGUE = {
  {
    id = "firstflight", title = "First Flight",
    brief = "Get something off the pad and bring the pod back in one piece.\n" ..
            "Ops isn't asking for elegance.",
    reward = 4500, rep = 1,
    goals = {
      { kind = "altitude", value = 2000, label = "Reach 2,000 m" },
      { kind = "land", label = "Land with the pod intact" },
    },
  },
  {
    id = "highroad", title = "The High Road",
    brief = "Twenty-five kilometres. The air is thin enough up there that the\n" ..
            "board wants to see the telemetry before they sign anything bigger.",
    reward = 9000, rep = 1, requires = "firstflight",
    goals = {
      { kind = "altitude", value = 25000, label = "Reach 25,000 m" },
      { kind = "land", label = "Land intact" },
    },
  },
  {
    id = "orbit", title = "Made Orbit",
    brief = "Not a lob — an ORBIT. Get your periapsis clear of the atmosphere\n" ..
            "so the craft comes back around on its own.",
    reward = 22000, rep = 2, requires = "highroad",
    goals = {
      { kind = "orbit", value = 20000, label = "Periapsis above 20 km" },
    },
  },
  {
    id = "rendezvous", title = "Rendezvous",
    brief = "Two craft, one seam. Undock a module in flight and latch it back\n" ..
            "on — the manoeuvre every station and every return trip is built of.",
    reward = 30000, rep = 2, requires = "orbit",
    goals = {
      { kind = "dock", label = "Dock two craft in flight" },
      { kind = "land", label = "Bring the stack home intact" },
    },
  },
  {
    id = "prospect", title = "Prospector",
    brief = "The board wants to know the rock is worth landing on. Mine ore,\n" ..
            "fly it home and put 30 units through the depot.",
    reward = 8000, rep = 1, requires = "firstflight",
    goals = {
      { kind = "deliver", class = "ore", value = 30,
        label = "30 units of ore in the warehouse" },
    },
  },
  {
    id = "botany", title = "Field Botany",
    brief = "Cut samples of whatever grows down there — the labs will take any\n" ..
            "plant matter, and the stranger it is the better they like it.",
    reward = 7000, rep = 1, requires = "firstflight",
    goals = {
      { kind = "deliver", class = "flora", value = 20,
        label = "20 units of plant matter in the warehouse" },
    },
  },
  {
    id = "deepcore", title = "Deep Core",
    brief = "Resonant crystal only forms twenty metres down and it does not\n" ..
            "come up with a shovel. Take the laser and dig.",
    reward = 26000, rep = 2, requires = "prospect",
    goals = {
      { kind = "sample", mat = "crystal", label = "Find resonant crystal" },
      { kind = "deliver", mat = "crystal", value = 5,
        label = "5 crystal in the warehouse" },
    },
  },
  {
    id = "recovery", title = "Bring It Home",
    brief = "Ops is tired of writing off hulls. Land near the base and RECOVER\n" ..
            "the craft — you get most of the hardware's value back.",
    reward = 12000, rep = 1, requires = "firstflight",
    goals = {
      { kind = "recover", label = "Recover a craft at the base" },
    },
  },
}

-- Published for the panels.
active = nil       -- the accepted contract's id
progress = ""      -- one-line summary of the active contract

local co = nil
local flight_seen = {}   -- goal index → true once met on THIS flight
local perm = {}          -- goal index → true once done, for good (see PERMANENT)
local last_piloting = false

-- Is goal `i` met? Either it latched on this flight, or it's one of the ones
-- you can't un-do.
local function met(i)
  return flight_seen[i] or perm[i] or false
end

local function company()
  if not co or not co.node or not co.node.valid then co = findScript("company") end
  return co
end

local function done_set()
  return save.get("mission.done") or {}
end

function byId(id)
  for _, m in ipairs(CATALOGUE) do
    if m.id == id then return m end
  end
  return nil
end

-- Every contract you could take right now: not already flown, prerequisite met,
-- and not the one already on the books.
function offers()
  local done = done_set()
  local out = {}
  for _, m in ipairs(CATALOGUE) do
    if not done[m.id] and m.id ~= active
      and (not m.requires or done[m.requires]) then
      out[#out + 1] = m
    end
  end
  return out
end

function completed()
  local done, out = done_set(), {}
  for _, m in ipairs(CATALOGUE) do
    if done[m.id] then out[#out + 1] = m end
  end
  return out
end

function accept(id)
  local m = byId(id)
  if not m or active == id then return false end
  active = id
  flight_seen, perm = {}, {}
  save.set("mission.active", id)
  save.set("mission.seen", {})
  save.set("mission.perm", {})
  log("CONTRACT ACCEPTED — " .. m.title)
  return true
end

function abandon()
  if not active then return false end
  local m = byId(active)
  active = nil
  flight_seen, perm = {}, {}
  save.set("mission.active", false)
  save.set("mission.seen", {})
  save.set("mission.perm", {})
  -- Walking away from a signed contract costs standing, not money.
  local c = company()
  if c and c.addRep then c.addRep(-1, "abandoned " .. ((m and m.title) or "contract")) end
  return true
end

-- The craft the player is actually flying, if any.
local function piloted()
  for _, v in ipairs(findScripts("vessel_controller")) do
    if v.piloting then return v end
  end
  return nil
end

-- Altitude above the dominant body's surface.
local function alt_of(v)
  local n = v.node
  if not n or not n.valid then return nil end
  local d = space.dominant(n.x, n.y, n.z)
  local b = d and space.body(d)
  if not b then return nil end
  local dx, dy, dz = n.x - b.x, n.y - b.y, n.z - b.z
  return math.sqrt(dx * dx + dy * dy + dz * dz) - b.radius, b
end

-- Is this goal satisfied RIGHT NOW? (Latching is the caller's job — a goal that
-- was true a moment ago stays true for the rest of the flight.)
local function goal_now(g, v, info)
  local a, body = alt_of(v)
  if g.kind == "altitude" then
    return a ~= nil and a >= g.value
  elseif g.kind == "speed" then
    if not info then return false end
    local s = info.vel
    return math.sqrt(s.x * s.x + s.y * s.y + s.z * s.z) >= g.value
  elseif g.kind == "orbit" then
    -- A real orbit: the periapsis has to clear the value, which is exactly the
    -- thing a lob can't fake. `space.elements` is the engine's own conic.
    local n = v.node
    local oe = n and space.elements and space.elements(n.x, n.y, n.z, n.vx, n.vy, n.vz)
    if not (oe and oe.periapsis and body) then return false end
    -- `periapsis` is a RADIUS from the body centre, and `e` (not `ecc`) is the
    -- eccentricity — an escape trajectory is not an orbit however high it goes.
    return (oe.e or 1.0) < 1.0 and (oe.periapsis - body.radius) >= g.value
  elseif g.kind == "land" then
    if not info or not info.grounded then return false end
    if g.body and body and body.name ~= g.body then return false end
    -- "Intact" means the pod is still aboard and the craft has stopped.
    local s = info.vel
    return math.sqrt(s.x * s.x + s.y * s.y + s.z * s.z) < 1.5
  elseif g.kind == "dock" then
    return (v.dock and (v.dock.latched or 0) > 0) or false
  elseif g.kind == "recover" then
    return save.get("mission.recovered") == true
  elseif g.kind == "deliver" then
    -- What's actually in the warehouse at home: mined or cut, flown back and
    -- put through the depot's crane. Selling it afterwards can't un-do the
    -- delivery, because these goals latch permanently.
    local inv, mats = findScript("inventory"), findScript("materials")
    if not inv then return false end
    if g.mat then return inv.count("base", g.mat) >= (g.value or 1) end
    local n = 0
    for _, it in ipairs(inv.items("base")) do
      if not g.class or (mats and mats.class(it.mat) == g.class) then n = n + it.n end
    end
    return n >= (g.value or 1)
  elseif g.kind == "sample" then
    local inv = findScript("inventory")
    return inv ~= nil and inv.seen(g.mat) == true
  end
  return false
end

-- Kinds that are things you DID rather than states of the flight you're on:
-- they latch for the whole contract and survive stepping out of the pod.
local PERMANENT = { deliver = true, sample = true, recover = true }

-- One line per goal, ✓ once met.
function goalLines()
  local m = active and byId(active)
  if not m then return {} end
  local out = {}
  for i, g in ipairs(m.goals) do
    out[#out + 1] = string.format("%s %s", met(i) and "✓" or "○", g.label)
  end
  return out
end

function activeLine()
  local m = active and byId(active)
  if not m then return nil end
  local n, total = 0, #m.goals
  for i = 1, total do
    if met(i) then n = n + 1 end
  end
  return string.format("%s  %d/%d", m.title, n, total)
end

local function payout(m)
  local c = company()
  if c then
    if c.earn then c.earn(m.reward, "contract: " .. m.title) end
    if c.addRep then c.addRep(m.rep or 0, "completed " .. m.title) end
  end
  local done = done_set()
  done[m.id] = true
  save.set("mission.done", done)
  active = nil
  flight_seen, perm = {}, {}
  save.set("mission.active", false)
  save.set("mission.seen", {})
  save.set("mission.perm", {})
  log(string.format("CONTRACT COMPLETE — %s   +$%d", m.title, m.reward))
end

function start(node)
  local a = save.get("mission.active")
  active = (type(a) == "string") and a or nil
  local seen = save.get("mission.seen")
  flight_seen = (type(seen) == "table") and seen or {}
  local pm = save.get("mission.perm")
  perm = (type(pm) == "table") and pm or {}
end

function update(node, dt)
  local m = active and byId(active)
  if not m then
    progress = ""
    return
  end
  -- The goals you can meet with both feet on the ground — a warehouse delivery,
  -- a discovery, a recovery — are checked every tick whether or not anyone is
  -- flying, and once true they stay true for the rest of the contract.
  for i, g in ipairs(m.goals) do
    if PERMANENT[g.kind] and not perm[i] and goal_now(g, { node = node }, nil) then
      perm[i] = true
      save.set("mission.perm", perm)
      log("  ✓ " .. g.label)
    end
  end

  local v = piloted()
  -- Leaving the craft (or losing it) ends the flight's latches: goals have to
  -- be met on ONE trip, which is what makes "get high AND land intact" a flight
  -- plan instead of two unrelated afternoons.
  if not v then
    if last_piloting and not save.get("mission.recovered") then
      flight_seen = {}
      save.set("mission.seen", {})
    end
    last_piloting = false
    progress = activeLine() or ""
  else
    last_piloting = true
    local info = assembly.info(v.node)
    for i, g in ipairs(m.goals) do
      if not PERMANENT[g.kind] and not flight_seen[i] and goal_now(g, v, info) then
        flight_seen[i] = true
        save.set("mission.seen", flight_seen)
        log("  ✓ " .. g.label)
      end
    end
    progress = activeLine() or ""
  end
  -- All goals met → pay out.
  local all = true
  for i = 1, #m.goals do
    if not met(i) then all = false end
  end
  if all then payout(m) end
end
