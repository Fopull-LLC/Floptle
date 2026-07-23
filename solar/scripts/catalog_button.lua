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

local el, builder

function start(node)
  el = node:getcomponent("UiElement")
end

function clicked(node)
  if not builder then builder = findScript("builder") end
  local id = IDS[math.floor(params.part)]
  if builder and builder.pick and id then builder.pick(id) end
end

function hoverStart(node)
  if el then el.opacity = 0.75 end
end

function hoverEnd(node)
  if el then el.opacity = 1.0 end
end
