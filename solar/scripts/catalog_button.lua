-- One CATALOGUE button in the ship builder: clicking hands its part id to the
-- builder (`pick`), which spawns the ghost. `params.part` indexes the id list
-- below so the Inspector stays a number field.
--
-- A button also shows its part's LOCK state: parts past the starter set need
-- research, and a locked card is dimmed, labelled with its price, and explains
-- itself in the hint bar on hover. Clicking a locked card buys the unlock (the
-- builder's `pick` does that) rather than doing nothing — the catalogue is the
-- research screen.

defaults = { part = 1 }

-- Append-only order: existing catalogue buttons keep their numbers, new parts
-- get the next indices (their buttons are added at the end of the panel).
local IDS = { "pod", "chute", "tankS", "tankM", "engineS", "engineM",
              "decoupler", "legs", "radialDec",
              "nose", "fins", "battery", "dish", "solar",
              "skipper", "radialTank", "dockPort", "rcs", "cargo" }

local el, builder, idle
local locked = false
local base_text = nil

local function id_of()
  return IDS[math.floor(params.part)]
end

function start(node)
  el = node:getcomponent("UiElement")
end

local function ensureIdle()
  if idle or not el then return end
  idle = { el.fillR or 0.1, el.fillG or 0.14, el.fillB or 0.18, el.fillA or 0.92 }
end

local function setFill(r, g, b, a)
  if el then el.fillR = r; el.fillG = g; el.fillB = b; el.fillA = a end
end

local function hoverFill()
  if not idle then return end
  setFill(
    math.min(1.0, idle[1] * 1.5 + 0.07),
    math.min(1.0, idle[2] * 1.45 + 0.09),
    math.min(1.0, idle[3] * 1.4 + 0.12),
    math.min(1.0, idle[4] + 0.05)
  )
  if el then el.border = 1.6; el.opacity = 1.0 end
end

-- Keep the card honest: name, price, and a LOCK marker while R&D hasn't opened
-- it. The authored text is the fallback (an unresolvable id keeps its label).
function update(node, dt)
  if not el then el = node:getcomponent("UiElement") end
  if not builder then builder = findScript("builder") end
  local rs = findScript("research")
  local id = id_of()
  if not (builder and builder.partInfo and id) then return end
  local def = builder.partInfo(id)
  if not def then return end
  if not base_text then base_text = def.label end
  local was = locked
  locked = (rs and not rs.isUnlocked(id)) or false
  local co = findScript("company")
  local price = locked and ((rs and rs.costOf(id)) or 0) or def.cost
  local money = (co and co.money) and co.money(price) or ("$" .. price)
  local text = locked and string.format("%s   LOCK %s", def.label, money)
    or string.format("%s   %s", def.label, money)
  if node.text ~= text then node.text = text end
  if was ~= locked or not idle then
    ensureIdle()
    -- Locked cards read as unavailable-but-live: dimmer and cooler, never
    -- invisible — you're supposed to want them.
    if locked then
      if el then el.opacity = 0.62 end
    elseif el then
      el.opacity = 1.0
    end
  end
end

function clicked(node)
  if not builder then builder = findScript("builder") end
  local id = id_of()
  if builder and builder.pick and id then builder.pick(id) end
end

function hoverStart(node)
  ensureIdle()
  hoverFill()
  local rs = findScript("research")
  local id = id_of()
  if rs and id and not rs.isUnlocked(id) and builder and builder.hintNow then
    builder.hintNow(rs.labelOf(id) .. "  —  " .. (rs.lockLine(id) or ""), 4.0)
  end
end

function hoverEnd(node)
  if idle then setFill(idle[1], idle[2], idle[3], idle[4]) end
  if el then el.border = 1.0; el.opacity = locked and 0.62 or 1.0 end
end

function pressed(node)
  ensureIdle()
  if idle then setFill(idle[1] * 0.7, idle[2] * 0.7, idle[3] * 0.7, math.min(1.0, idle[4] + 0.05)) end
end

function released(node)
  hoverFill()
end
