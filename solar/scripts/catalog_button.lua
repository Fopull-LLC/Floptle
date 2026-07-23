-- One CATALOGUE button in the ship builder: clicking hands its part id to the
-- builder (`pick`), which spawns the ghost. `params.part` indexes the id list
-- below so the Inspector stays a number field.

defaults = { part = 1 }

-- Append-only order: existing catalogue buttons keep their numbers, new parts
-- get the next indices (their buttons are added at the end of the panel).
local IDS = { "pod", "chute", "tankS", "tankM", "engineS", "engineM",
              "decoupler", "legs", "radialDec",
              "nose", "fins", "battery", "dish", "solar",
              "skipper", "radialTank" }

local el, builder, idle

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

function clicked(node)
  if not builder then builder = findScript("builder") end
  local id = IDS[math.floor(params.part)]
  if builder and builder.pick and id then builder.pick(id) end
end

function hoverStart(node)
  ensureIdle()
  hoverFill()
end

function hoverEnd(node)
  if idle then setFill(idle[1], idle[2], idle[3], idle[4]) end
  if el then el.border = 1.0; el.opacity = 1.0 end
end

function pressed(node)
  ensureIdle()
  if idle then setFill(idle[1] * 0.7, idle[2] * 0.7, idle[3] * 0.7, math.min(1.0, idle[4] + 0.05)) end
end

function released(node)
  hoverFill()
end
