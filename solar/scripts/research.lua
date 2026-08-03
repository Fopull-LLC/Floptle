-- RESEARCH — what the catalogue is still holding back, and what it costs to
-- open it.
--
-- The company starts able to build a rocket that reaches the sky and comes back
-- down. Everything past that — bigger tanks, real engines, power, comms, docking
-- ports, cargo bays — is LOCKED, and each lock names its own price:
--
--   money        research is paid for out of the same account as hulls
--   a material   you must have HELD the stuff (see it in the inventory's
--                discovery set) before anyone will develop it
--   standing     some things nobody sells to a company with a bad record
--   another tech the obvious ladders (docking before cargo)
--
-- That is the whole point of the material economy: mining and harvesting are
-- how you pay for, and qualify for, the next part. There is no separate research
-- screen — a locked card in the builder tells you exactly what it wants and
-- unlocks on the spot when you click it, which is where you're standing when you
-- want it.
--
--     local rs = findScript("research")
--     rs.isUnlocked("dockPort")      false
--     rs.lockLine("dockPort")        "LOCKED · $900 · needs Copper Ore"
--     rs.unlock("dockPort")          → true, or false + the reason

defaults = {}

-- Anything NOT in this table ships unlocked. Keep the starter set (pod, nose,
-- chute, small tank, small engine, fins, decoupler, legs) out of it on purpose:
-- the first flight must be buildable with no research at all.
TECH = {
  tankM      = { cost = 260,  label = "FT-M Tank" },
  battery    = { cost = 320,  label = "Battery",        mat = "copper" },
  engineM    = { cost = 480,  label = "Anvil Engine" },
  dish       = { cost = 420,  label = "Comms Dish",     mat = "copper" },
  radialDec  = { cost = 300,  label = "Radial Decoupler" },
  radialTank = { cost = 340,  label = "Radial Tank",    after = "tankM" },
  solar      = { cost = 640,  label = "Solar Panel",    mat = "silica",  after = "battery" },
  cargo      = { cost = 520,  label = "Cargo Bay",      mat = "iron" },
  skipper    = { cost = 1250, label = "Skipper Engine", mat = "iron",    after = "engineM" },
  rcs        = { cost = 700,  label = "RCS Block",      after = "engineM" },
  dockPort   = { cost = 950,  label = "Docking Port",   mat = "copper",  rep = 1, after = "rcs" },
}

-- Published for panels.
spent = 0

local function done()
  return save.get("res.done") or {}
end

function isUnlocked(id)
  if not TECH[id] then return true end
  return done()[id] == true
end

function costOf(id)
  local t = TECH[id]
  return t and t.cost or 0
end

function labelOf(id)
  local t = TECH[id]
  return t and t.label or id
end

-- What's standing in the way, as a sentence — or nil when nothing is.
function blockedBy(id)
  local t = TECH[id]
  if not t or isUnlocked(id) then return nil end
  if t.after and not isUnlocked(t.after) then
    return "needs " .. labelOf(t.after)
  end
  if t.mat then
    local inv, mats = findScript("inventory"), findScript("materials")
    if inv and not inv.seen(t.mat) then
      return "needs a sample of " .. ((mats and mats.name(t.mat)) or t.mat)
    end
  end
  if t.rep then
    local co = findScript("company")
    if co and co.rep() < t.rep then
      return string.format("needs reputation %+d", t.rep)
    end
  end
  return nil
end

-- The one line a locked catalogue card shows. Reads as an instruction, never as
-- a "no": either what to go and find, or what to click.
function lockLine(id)
  if isUnlocked(id) then return nil end
  local co = findScript("company")
  local money = co and co.money(costOf(id)) or ("$" .. costOf(id))
  local why = blockedBy(id)
  if why then return "LOCKED · " .. why end
  local afford = (not co) or co.afford(costOf(id))
  return string.format("LOCKED · research %s%s", money, afford and "  (click)" or "  — not enough funds")
end

-- Buy the unlock. Returns ok, reason.
function unlock(id)
  if isUnlocked(id) then return true, "already unlocked" end
  local why = blockedBy(id)
  if why then return false, why end
  local co = findScript("company")
  local cost = costOf(id)
  if co and not co.spend(cost, "research: " .. labelOf(id)) then
    return false, "not enough funds"
  end
  local d = done()
  d[id] = true
  save.set("res.done", d)
  save.set("res.spent", (save.get("res.spent") or 0) + cost)
  spent = save.get("res.spent")
  log("RESEARCH COMPLETE: " .. labelOf(id))
  if co then co.addRep(1, "research: " .. labelOf(id)) end
  return true, labelOf(id)
end

-- Called by the depot when a first sale lands: a discovery can turn a "needs a
-- sample of…" lock into a purchasable one, and the player should hear about it
-- rather than find out by walking back to the builder.
function noteDiscovery()
  local ready = {}
  for id in pairs(TECH) do
    if not isUnlocked(id) and not blockedBy(id) then ready[#ready + 1] = labelOf(id) end
  end
  table.sort(ready)
  if #ready > 0 then
    log("R&D: ready to research — " .. table.concat(ready, ", "))
  end
  return ready
end

-- Everything still locked, with its reason — the "what should I do next" list.
function pendingLines()
  local out = {}
  local ids = {}
  for id in pairs(TECH) do ids[#ids + 1] = id end
  table.sort(ids, function(a, b) return TECH[a].cost < TECH[b].cost end)
  for _, id in ipairs(ids) do
    if not isUnlocked(id) then
      out[#out + 1] = string.format("%-18s %s", labelOf(id), lockLine(id))
    end
  end
  return out
end
