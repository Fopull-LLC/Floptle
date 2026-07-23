-- A button inside the facilities panel. `params.action`: 1 = the context action
-- (Open Builder / Return to Menu), 2 = close the panel. Routes into facility_menu.

defaults = { action = 1 }

local el, menu, idle

function start(node)
  el = node:getcomponent("UiElement")
end

-- Cache the authored idle fill the first time we're hovered (the mirror is live
-- by then and the fill hasn't been touched yet), so hover/press restore to THIS
-- button's own color — blue action, red close, green launch, etc.
local function ensureIdle()
  if idle or not el then return end
  idle = { el.fillR or 0.12, el.fillG or 0.2, el.fillB or 0.28, el.fillA or 0.95 }
end

local function setFill(r, g, b, a)
  if el then el.fillR = r; el.fillG = g; el.fillB = b; el.fillA = a end
end

local function idleFill()
  if idle then setFill(idle[1], idle[2], idle[3], idle[4]) end
  if el then el.border = 1.0; el.opacity = 1.0 end
end

local function hoverFill()
  if not idle then return end
  setFill(
    math.min(1.0, idle[1] * 1.45 + 0.06),
    math.min(1.0, idle[2] * 1.4 + 0.07),
    math.min(1.0, idle[3] * 1.35 + 0.09),
    math.min(1.0, idle[4] + 0.05)
  )
  if el then el.border = 1.6; el.opacity = 1.0 end
end

function clicked(node)
  if not menu then menu = findScript("facility_menu") end
  if not menu then return end
  local a = math.floor(params.action)
  if a == 1 and menu.action then menu.action() end
  if a == 2 and menu.close then menu.close() end
end

function hoverStart(node)
  ensureIdle()
  hoverFill()
end

function hoverEnd(node)
  idleFill()
end

function pressed(node)
  ensureIdle()
  if idle then setFill(idle[1] * 0.7, idle[2] * 0.7, idle[3] * 0.7, math.min(1.0, idle[4] + 0.05)) end
end

function released(node)
  hoverFill()
end
